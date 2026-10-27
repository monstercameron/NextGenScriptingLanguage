use flow::ast::StepKind;
use flow::{manifest::Registry, parser, runtime};
use serde_json::{json, Value as Json};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

fn options(cwd: &Path, max_output_bytes: usize) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes,
        command_timeout: Some(Duration::from_secs(10)),
        total_timeout: Some(Duration::from_secs(15)),
        ..runtime::RunOptions::new(cwd.to_owned())
    }
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

#[cfg(unix)]
fn await_file(path: &Path) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    path.is_file()
}

#[cfg(unix)]
fn write_job_module(catalog: &Path, name: &str, stdout: &str) {
    std::fs::create_dir_all(catalog).unwrap();
    let source_name = format!("{}.program", name.replace('.', "_"));
    let source = format!(
        "define a program named entry\n  spawn as output\n    run \"printf\" with arguments [\"%s\", {}]\n  await output\n  then encode \"ok\" as json\n",
        quote(stdout),
    );
    std::fs::write(catalog.join(&source_name), source).unwrap();
    let manifest = json!({
        "name": name,
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": source_name,
                "entry": "entry",
                "interface_version": 1
            },
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "json"}
        }
    });
    std::fs::write(
        catalog.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[cfg(unix)]
fn write_active_job_module(
    catalog: &Path,
    name: &str,
    marker_prefix: &Path,
    gate: &Path,
) -> Vec<PathBuf> {
    std::fs::create_dir_all(catalog).unwrap();
    let source_name = format!("{}.program", name.replace('.', "_"));
    let markers = (0..8)
        .map(|index| marker_prefix.with_extension(format!("job{index}")))
        .collect::<Vec<_>>();
    let mut source = String::from("define a program named entry\n");
    for (index, marker) in markers.iter().enumerate() {
        source.push_str(&format!(
            "  spawn as job{index}\n    run \"sh\" with arguments [\"-c\", \"touch $1; while [ ! -e $2 ]; do sleep 0.01; done\", \"job\", {}, {}]\n",
            quote(&marker.to_string_lossy()),
            quote(&gate.to_string_lossy()),
        ));
    }
    source.push_str("  await job0\n");
    for index in 1..8 {
        source.push_str(&format!("  cancel job{index}\n"));
    }
    source.push_str("  then encode \"ok\" as json\n");
    std::fs::write(catalog.join(&source_name), source).unwrap();
    let manifest = json!({
        "name": name,
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": source_name,
                "entry": "entry",
                "interface_version": 1
            },
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "json"}
        }
    });
    std::fs::write(
        catalog.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    markers
}

#[test]
fn parser_lowers_the_four_named_job_operations_without_changing_legacy_ast() {
    let program = parser::parse(
        "spawn as build\n  run \"printf\" with arguments [\"x\"]\ninspect build as state\nawait build and keep status as code and keep result as result\ncancel build\n",
    )
    .unwrap();
    assert!(matches!(program.steps[0].kind, StepKind::SpawnJob { .. }));
    assert!(matches!(program.steps[1].kind, StepKind::InspectJob { .. }));
    assert!(matches!(program.steps[2].kind, StepKind::AwaitJob { .. }));
    assert!(matches!(program.steps[3].kind, StepKind::CancelJob { .. }));

    let encoded = serde_json::to_vec(&program).unwrap();
    let decoded: flow::ast::Program = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, program);
}

#[cfg(unix)]
#[test]
fn loop_iterations_can_spawn_and_await_the_same_named_job() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "repeat 3 times\n  spawn as item\n    run \"printf\" with arguments [\"item\"]\n  await item\n",
    )?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;

    assert_eq!(result.launched_processes, 3);
    assert_eq!(result.output, b"item");
    Ok(())
}

#[cfg(unix)]
#[test]
fn loop_iterations_can_collect_same_named_jobs_for_a_later_await() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "repeat 3 times\n  spawn as item\n    run \"printf\" with arguments [\"item\"]\nawait item and keep status as code and keep result as results\nthen encode [code, results] as json\n",
    )?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;

    let value: Json = serde_json::from_slice(&result.output)?;
    assert_eq!(result.launched_processes, 3);
    assert_eq!(value[0], json!(0));
    assert_eq!(value[1].as_array().map(Vec::len), Some(3));
    assert_eq!(value[1][0]["stdout"], json!([105, 116, 101, 109]));
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_loop_job_batches_are_collected_into_the_outer_batch() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "repeat 2 times\n  repeat 2 times\n    spawn as item\n      run \"printf\" with arguments [\"x\"]\nawait item\n",
    )?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;

    assert_eq!(result.launched_processes, 4);
    assert_eq!(result.output, b"xxxx");
    Ok(())
}

#[cfg(unix)]
#[test]
fn continue_collects_current_iteration_job_for_later_await() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "repeat 3 times\n  spawn as item\n    run \"printf\" with arguments [\"x\"]\n  continue\nawait item\n",
    )?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;

    assert_eq!(result.launched_processes, 3);
    assert_eq!(result.output, b"xxx");
    Ok(())
}

#[test]
fn loop_collected_job_batch_must_still_be_observed_before_scope_exit() {
    let program = parser::parse("repeat 3 times\n  spawn as item\n    run \"true\"\n").unwrap();

    let error = runtime::validate(&program, &Registry::default())
        .expect_err("a collected loop batch must be observed before scope exit");
    assert!(format!("{error:#}").contains("every background job must be awaited or cancelled"));
}

#[cfg(unix)]
#[test]
fn cancelling_loop_batch_signals_all_jobs_before_joining_first() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let slow_pid = directory.path().join("slow-job.pid");
    let fast_pid = directory.path().join("fast-job.pid");
    let ready = directory.path().join("fast-job-ready");
    let program = parser::parse(&format!(
        r#"for each phase in ["slow", "fast"]
  spawn as worker
    run "sh" with arguments ["-c", "if [ \"$1\" = slow ]; then echo $$ > \"$2\"; trap '' TERM; while :; do sleep 0.01; done; else echo $$ > \"$3\"; touch \"$4\"; while :; do sleep 0.01; done; fi", "job", phase, {}, {}, {}]
run "sh" with arguments ["-c", "while [ ! -f \"$1\" ]; do sleep 0.01; done", "wait", {}]
cancel worker
"#,
        quote(&slow_pid.to_string_lossy()),
        quote(&fast_pid.to_string_lossy()),
        quote(&ready.to_string_lossy()),
        quote(&ready.to_string_lossy()),
    ))?;

    let observed_violation = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer_violation = std::sync::Arc::clone(&observed_violation);
    let observer_slow_pid = slow_pid.clone();
    let observer_fast_pid = fast_pid.clone();
    let observer = std::thread::spawn(move || {
        if !await_file(&observer_slow_pid) || !await_file(&observer_fast_pid) {
            return;
        }
        let Ok(pid) = std::fs::read_to_string(&observer_slow_pid) else {
            return;
        };
        let Ok(pid) = pid.trim().parse::<libc::pid_t>() else {
            return;
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            // SAFETY: this probes the live PID written by the test-owned child.
            if unsafe { libc::kill(pid, 0) } == -1 {
                let Ok(fast_pid) = std::fs::read_to_string(&observer_fast_pid) else {
                    observer_violation.store(true, std::sync::atomic::Ordering::Release);
                    return;
                };
                let Ok(fast_pid) = fast_pid.trim().parse::<libc::pid_t>() else {
                    observer_violation.store(true, std::sync::atomic::Ordering::Release);
                    return;
                };
                // SAFETY: this probes the fast child PID written by the test-owned child.
                if unsafe { libc::kill(fast_pid, 0) } == 0 {
                    observer_violation.store(true, std::sync::atomic::Ordering::Release);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;
    assert!(result.output.is_empty());
    observer.join().expect("cancellation observer panicked");
    assert!(
        !observed_violation.load(std::sync::atomic::Ordering::Acquire),
        "a later loop job remained running while the first job was being reaped"
    );
    let fast_pid: libc::pid_t = std::fs::read_to_string(&fast_pid)?.trim().parse()?;
    // SAFETY: this probes the child PID written by the test-owned fast job.
    assert_eq!(unsafe { libc::kill(fast_pid, 0) }, -1);
    Ok(())
}

#[cfg(unix)]
#[test]
fn try_catch_cancels_and_reaps_jobs_owned_by_the_try_body() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("job-started");
    let pid = directory.path().join("job.pid");
    let program = parser::parse(&format!(
        "try\n  spawn as worker\n    run \"sh\" with arguments [\"-c\", \"touch \\\"$1\\\"; echo $$ > \\\"$2\\\"; while :; do sleep 1; done\", \"job\", {}, {}]\n  run \"sh\" with arguments [\"-c\", \"while [ ! -f \\\"$1\\\" ]; do sleep 0.01; done\", \"wait\", {}]\n  run \"false\" with arguments []\n  cancel worker\ncatch failure\n  run \"sh\" with arguments [\"-c\", \"if kill -0 \\\"$(cat \\\"$1\\\")\\\" 2>/dev/null; then exit 1; fi\", \"check\", {}]\n  run \"printf\" with arguments [\"caught\"]\n",
        quote(&marker.to_string_lossy()),
        quote(&pid.to_string_lossy()),
        quote(&marker.to_string_lossy()),
        quote(&pid.to_string_lossy()),
    ))?;

    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;
    assert_eq!(result.output, b"caught");
    Ok(())
}

#[cfg(unix)]
#[test]
fn catch_body_jobs_are_reaped_if_the_catch_fails_before_await() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("catch-job-started");
    let pid = directory.path().join("catch-job.pid");
    let program = parser::parse(&format!(
        "try\n  run \"false\" with arguments []\ncatch failure\n  spawn as worker\n    run \"sh\" with arguments [\"-c\", \"echo $$ > \\\"$2\\\"; touch \\\"$1\\\"; exec sleep 30\", \"job\", {}, {}]\n  run \"sh\" with arguments [\"-c\", \"while [ ! -f \\\"$1\\\" ]; do sleep 0.01; done\", \"wait\", {}]\n  run \"false\" with arguments []\n  await worker\n",
        quote(&marker.to_string_lossy()),
        quote(&pid.to_string_lossy()),
        quote(&marker.to_string_lossy()),
    ))?;

    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    );
    assert!(
        result.is_err(),
        "the catch body deliberately fails before its await"
    );
    let job_pid: libc::pid_t = std::fs::read_to_string(&pid)?.trim().parse()?;
    // SAFETY: this probes the child PID written by the catch-owned job above.
    assert_eq!(
        unsafe { libc::kill(job_pid, 0) },
        -1,
        "catch job remained alive"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_call_failure_reaps_jobs_owned_by_the_caller_try() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("caller-try-job-started");
    let pid = directory.path().join("caller-try-job.pid");
    let program = parser::parse(&format!(
        "define a program named failing\n  run \"false\" with arguments []\ntry\n  spawn as worker\n    run \"sh\" with arguments [\"-c\", \"echo $$ > \\\"$2\\\"; touch \\\"$1\\\"; exec sleep 30\", \"job\", {}, {}]\n  run \"sh\" with arguments [\"-c\", \"while [ ! -f \\\"$1\\\" ]; do sleep 0.01; done\", \"wait\", {}]\n  run program failing\n  await worker\ncatch failure\n  run \"sh\" with arguments [\"-c\", \"if kill -0 \\\"$(cat \\\"$1\\\")\\\" 2>/dev/null; then exit 1; fi\", \"check\", {}]\n  then encode \"caught\" as json\n",
        quote(&marker.to_string_lossy()),
        quote(&pid.to_string_lossy()),
        quote(&marker.to_string_lossy()),
        quote(&pid.to_string_lossy()),
    ))?;

    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;
    assert_eq!(result.output, b"\"caught\"");
    Ok(())
}

#[cfg(unix)]
#[test]
fn sigterm_to_service_reaps_service_setup_jobs() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};

    let directory = tempfile::tempdir()?;
    let pid_path = directory.path().join("service-job.pid");
    let source = format!(
        "spawn as service_worker\n  run \"sh\" with arguments [\"-c\", \"echo $$ > \\\"$1\\\"; exec sleep 30\", \"job\", {}]\nevery 10 milliseconds\n  run \"true\"\n",
        quote(&pid_path.to_string_lossy()),
    );
    std::fs::write(directory.path().join("service.program"), source)?;
    let mut service = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("service.program")
        .current_dir(directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if !await_file(&pid_path) {
        let _ = service.kill();
        let _ = service.wait();
        anyhow::bail!("service-owned setup job did not start");
    }
    let job_pid: libc::pid_t = std::fs::read_to_string(&pid_path)?.trim().parse()?;
    // SAFETY: the PID is the child service process started by this test.
    assert_eq!(
        unsafe { libc::kill(service.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while service.try_wait()?.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if service.try_wait()?.is_none() {
        let _ = service.kill();
        let _ = service.wait();
        panic!("service did not stop after SIGTERM");
    }
    // SAFETY: this probes the PID written by the service-owned job started above.
    assert_eq!(
        unsafe { libc::kill(job_pid, 0) },
        -1,
        "service job remained alive"
    );
    Ok(())
}

#[test]
fn listener_handler_can_spawn_and_await_a_named_job() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;
    use std::process::{Command, Stdio};

    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("listener.program"),
        "listen to stdin\nwhenever a line arrives as record\n  spawn as worker\n    run \"printf\" with arguments [record]\n  await worker\n",
    )?;
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("listener.program")
        .arg("--max-runs")
        .arg("1")
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    process
        .stdin
        .take()
        .context("opening listener stdin")?
        .write_all(b"listener-output\n")?;
    let output = process.wait_with_output()?;
    assert!(
        output.status.success(),
        "listener failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"listener-output\n");
    Ok(())
}

#[cfg(unix)]
#[test]
fn tui_handler_jobs_are_owned_and_reaped_before_return() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "spawn as handler_job\n  run \"printf\" with arguments [\"handler\"]\nawait handler_job\n",
    )?;
    let result = runtime::run_with_context(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
        std::collections::BTreeMap::new(),
        Vec::new(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    assert_eq!(result.output, b"handler");
    assert_eq!(result.launched_processes, 1);
    Ok(())
}

#[cfg(unix)]
#[test]
fn awaited_pipeline_keeps_exact_bytes_and_repeated_await_is_idempotent() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let registry = Registry::default();
    let program = parser::parse(
        r#"spawn as bytes
  run "printf" with arguments ["%b", "\\377x"]
  pipe to "cat"
await bytes and keep status as code and keep result as record
then save to "first.bin"
await bytes
then encode record as json
"#,
    )?;
    let result = runtime::run(&program, &registry, &options(directory.path(), 1024 * 1024))?;

    assert_eq!(result.launched_processes, 2);
    assert_eq!(
        std::fs::read(directory.path().join("first.bin"))?,
        [0xff, b'x']
    );
    let record: Json = serde_json::from_slice(&result.output)?;
    assert_eq!(record["status"], json!(0));
    assert_eq!(record["stdout"], json!([255, 120]));
    assert_eq!(record["stdout_truncated"], json!(false));
    assert_eq!(record["stderr_truncated"], json!([false, false]));
    assert_eq!(record["stderr_limit_per_stage"], json!(65536));
    assert_eq!(record["stages"].as_array().unwrap().len(), 2);
    Ok(())
}

#[cfg(unix)]
#[test]
fn job_snapshots_its_working_directory_and_closes_stdin() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("first");
    let captured = directory.path().join("captured-cwd.txt");
    std::fs::create_dir(&first)?;
    let source = format!(
        "use folder {}\nspawn as cwd\n  run \"pwd\"\nspawn as no_input\n  run \"cat\"\nawait cwd\nthen save to {}\nawait no_input\n",
        quote(&first.to_string_lossy()),
        quote(&captured.to_string_lossy()),
    );
    let program = parser::parse(&source)?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )?;

    let expected_cwd = format!("{}\n", std::fs::canonicalize(&first)?.display()).into_bytes();
    assert_eq!(std::fs::read(captured)?, expected_cwd);
    assert!(result.output.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn run_wide_job_output_obeys_the_retention_cap() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "spawn as first\n  run \"head\" with arguments [\"-c\", \"80\", \"/dev/zero\"]\nawait first\nspawn as second\n  run \"head\" with arguments [\"-c\", \"80\", \"/dev/zero\"]\nawait second\n",
    )?;
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 128),
    )
    .err()
    .expect("aggregate job output must remain bounded");
    assert!(format!("{error:#}").contains("run-wide limit of"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn finite_module_called_from_finally_after_retired_jobs_succeeds() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = directory.path().join("applets");
    write_job_module(&catalog, "test.cleanup", "cleanup");
    let mut source = String::new();
    for index in 0..32 {
        source.push_str(&format!(
            "spawn as outer{index}\n  run \"true\"\nawait outer{index}\n"
        ));
    }
    source.push_str("try\n  run \"true\"\nfinally\n  run test.cleanup\n");
    let program = parser::parse(&source)?;
    runtime::run(
        &program,
        &Registry::load(&catalog)?,
        &options(directory.path(), 1024),
    )?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn service_handlers_can_use_jobs_from_imported_finite_modules() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = directory.path().join("applets");
    std::fs::create_dir_all(&catalog)?;
    let effect = directory.path().join("service-module-ran");
    let source_name = "job.program";
    std::fs::write(
        catalog.join(source_name),
        format!(
            "define a program named entry\n  spawn as effect\n    run \"touch\" with arguments [{}]\n  await effect\n  then encode \"ok\" as json\n",
            quote(&effect.to_string_lossy()),
        ),
    )?;
    let manifest = json!({
        "name": "test.job",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": source_name,
                "entry": "entry",
                "interface_version": 1
            },
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "json"}
        }
    });
    std::fs::write(
        catalog.join("test.job.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let registry = Registry::load(&catalog)?;
    let service = parser::parse("every 10 milliseconds\n  run test.job\n")?;
    let result = runtime::run_service(
        &service,
        &registry,
        &options(directory.path(), 1024),
        &runtime::ServiceLimits {
            max_runs: Some(1),
            max_record_bytes: 1024,
        },
        |_| Ok(()),
    )?;
    assert_eq!(result.runs, 1);
    assert!(
        effect.exists(),
        "the imported module's job should run within the handler"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn service_setup_preflights_all_jobs_before_starting_any() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let effect = directory.path().join("service-job-started");
    let missing = directory.path().join("missing-service-command");
    let program = parser::parse(&format!(
        "spawn as first\n  run \"touch\" with arguments [{}]\nspawn as second\n  run {}\nevery 10 milliseconds\n  run \"true\"\n",
        quote(&effect.to_string_lossy()),
        quote(&missing.to_string_lossy()),
    ))?;
    let error = runtime::run_service(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
        &runtime::ServiceLimits {
            max_runs: Some(1),
            max_record_bytes: 1024,
        },
        |_| Ok(()),
    )
    .err()
    .expect("the missing setup executable must fail preflight");
    assert!(format!("{error:#}").contains("executables unavailable"));
    assert!(
        !effect.exists(),
        "an earlier service-owned job must not start before all jobs preflight"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_finite_program_shares_the_active_job_cap_before_effects() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let child_effect = directory.path().join("nested-child-started");
    let mut source = format!(
        "define a program named nested\n  spawn as overflow\n    run \"touch\" with arguments [{}]\n  await overflow\n",
        quote(&child_effect.to_string_lossy()),
    );
    for index in 0..16 {
        source.push_str(&format!(
            "spawn as outer{index}\n  run \"sleep\" with arguments [\"30\"]\n"
        ));
    }
    source.push_str("run program nested\n");
    for index in 0..16 {
        source.push_str(&format!("cancel outer{index}\n"));
    }

    let program = parser::parse(&source)?;
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )
    .err()
    .expect("nested call must share the root run's active-job cap");
    assert!(format!("{error:#}").contains("concurrency limit (16)"));
    assert!(
        !child_effect.exists(),
        "a nested job beyond the admission limit must not start"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_finite_program_starts_after_prior_jobs_are_retired() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let child_effect = directory.path().join("nested-child-started");
    let mut source = format!(
        "define a program named nested\n  spawn as overflow\n    run \"touch\" with arguments [{}]\n  await overflow\n",
        quote(&child_effect.to_string_lossy()),
    );
    for index in 0..32 {
        source.push_str(&format!(
            "spawn as outer{index}\n  run \"true\"\nawait outer{index}\n"
        ));
    }
    source.push_str("run program nested\n");

    let program = parser::parse(&source)?;
    runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )?;
    assert!(child_effect.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_finite_program_shares_the_run_wide_stdout_budget() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let nested_stdout = "x".repeat(64);
    let source = format!(
        "define a program named nested\n  spawn as second\n    run \"printf\" with arguments [\"%s\", {}]\n  await second\nspawn as first\n  run \"printf\" with arguments [\"%s\", {}]\nawait first\nrun program nested\n",
        quote(&nested_stdout),
        quote(&"y".repeat(80)),
    );
    let program = parser::parse(&source)?;
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 128),
    )
    .err()
    .expect("nested calls must share the aggregate retained-stdout cap");
    assert!(format!("{error:#}").contains("run-wide limit of"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn concurrent_task_modules_share_the_run_wide_stdout_budget() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = directory.path().join("applets");
    write_job_module(&catalog, "test.first", &"x".repeat(120));
    write_job_module(&catalog, "test.second", &"y".repeat(120));
    let registry = Registry::load(&catalog)?;
    let source = format!(
        "spawn as initial\n  run \"printf\" with arguments [\"%s\", {}]\nawait initial\nparallel at most 2 as results\n  run test.first as first\n  run test.second as second\n",
        quote(&"p".repeat(400)),
    );
    let program = parser::parse(&source)?;
    let error = runtime::run(&program, &registry, &options(directory.path(), 512))
        .err()
        .expect("parallel program modules must share the root stdout budget");
    assert!(format!("{error:#}").contains("run-wide limit of"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn concurrent_task_modules_share_the_run_wide_active_job_cap() -> anyhow::Result<()> {
    use std::thread;
    use std::time::Instant;

    let directory = tempfile::tempdir()?;
    let catalog = directory.path().join("applets");
    let gate = directory.path().join("release-jobs");
    let first_markers = write_active_job_module(
        &catalog,
        "test.first",
        &directory.path().join("first"),
        &gate,
    );
    let second_markers = write_active_job_module(
        &catalog,
        "test.second",
        &directory.path().join("second"),
        &gate,
    );
    let registry = Registry::load(&catalog)?;
    let source = "spawn as initial\n  run \"sleep\" with arguments [\"30\"]\nparallel at most 2 as results\n  run test.first as first\n  run test.second as second\ncancel initial\n";
    let program = parser::parse(source)?;
    let root = directory.path().to_owned();
    let (sender, receiver) = std::sync::mpsc::channel();
    let runner = thread::spawn(move || {
        let result = runtime::run(&program, &registry, &options(&root, 1024 * 1024));
        let _ = sender.send(result);
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut result = None;
    loop {
        match receiver.try_recv() {
            Ok(completed) => {
                result = Some(completed);
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("task-module job runner exited without a result")
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        let all_children_started = first_markers.iter().all(|marker| marker.exists())
            && second_markers.iter().all(|marker| marker.exists());
        if all_children_started || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    if result.is_none() {
        std::fs::write(&gate, b"release")?;
        result = Some(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| {
                    anyhow::anyhow!("task-module job runner did not finish: {error}")
                })?,
        );
    }
    runner
        .join()
        .map_err(|_| anyhow::anyhow!("task-module job runner panicked"))?;
    let error = result
        .expect("task-module result should be available after the runner joins")
        .err()
        .expect("concurrent task modules must share the root active-job cap");
    assert!(format!("{error:#}").contains("concurrency limit (16)"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn spawn_acknowledges_a_running_job_before_later_statements_finish() -> anyhow::Result<()> {
    use std::thread;

    let directory = tempfile::tempdir()?;
    let started = directory.path().join("started");
    let gate = directory.path().join("gate");
    let start_script = r#"touch "$1"; while [ ! -e "$2" ]; do sleep 0.01; done; printf '\377done'"#;
    let source = format!(
        "spawn as held\n  run \"sh\" with arguments [\"-c\", {}, \"job\", {}, {}]\ninspect held as snapshot\nthen encode snapshot as json\nthen save to \"snapshot.json\"\nawait held\n",
        quote(start_script),
        quote(&started.to_string_lossy()),
        quote(&gate.to_string_lossy()),
    );
    let program = parser::parse(&source)?;
    let root = directory.path().to_owned();
    let runner = thread::spawn(move || {
        runtime::run(&program, &Registry::default(), &options(&root, 1024 * 1024))
    });

    let started_before_release = await_file(&started);
    let snapshot_path = directory.path().join("snapshot.json");
    let snapshot_written_before_release = await_file(&snapshot_path);
    let snapshot = if snapshot_written_before_release {
        serde_json::from_slice::<Json>(&std::fs::read(&snapshot_path)?)?
    } else {
        Json::Null
    };
    // Always release the worker before joining so a failed assertion cannot
    // strand the test thread or an owned child process.
    std::fs::write(&gate, b"release")?;
    let result = runner
        .join()
        .map_err(|_| anyhow::anyhow!("background job runner panicked"))??;

    assert!(started_before_release);
    assert!(snapshot_written_before_release);
    assert_eq!(snapshot["state"], json!("running"));
    assert!(snapshot["stages"][0]["pid"].as_i64().is_some());
    assert_eq!(snapshot["stages"][0]["status"], Json::Null);
    assert_eq!(result.output, [0xff, b'd', b'o', b'n', b'e']);
    Ok(())
}

#[cfg(unix)]
#[test]
fn cancelling_one_job_reaps_it_without_cancelling_its_sibling() -> anyhow::Result<()> {
    use std::thread;

    let directory = tempfile::tempdir()?;
    let first_started = directory.path().join("first-started");
    let first_gate = directory.path().join("first-gate");
    let second_started = directory.path().join("second-started");
    let second_gate = directory.path().join("second-gate");
    let first_script = r#"touch "$1"; while [ ! -e "$2" ]; do sleep 0.01; done; printf first"#;
    let second_script = r#"touch "$1"; while [ ! -e "$2" ]; do sleep 0.01; done; printf second"#;
    let wait_script = r#"while [ ! -e "$1" ]; do sleep 0.01; done"#;
    let source = format!(
        "spawn as first\n  run \"sh\" with arguments [\"-c\", {}, \"job\", {}, {}]\nrun \"sh\" with arguments [\"-c\", {}, \"wait\", {}]\nspawn as second\n  run \"sh\" with arguments [\"-c\", {}, \"job\", {}, {}]\nrun \"sh\" with arguments [\"-c\", {}, \"wait\", {}]\ncancel first\ninspect first as first_state\nthen encode first_state as json\nthen save to \"first.json\"\ninspect second as second_state\nthen encode second_state as json\nthen save to \"second.json\"\nawait second\n",
        quote(first_script),
        quote(&first_started.to_string_lossy()),
        quote(&first_gate.to_string_lossy()),
        quote(wait_script),
        quote(&first_started.to_string_lossy()),
        quote(second_script),
        quote(&second_started.to_string_lossy()),
        quote(&second_gate.to_string_lossy()),
        quote(wait_script),
        quote(&second_started.to_string_lossy()),
    );
    let program = parser::parse(&source)?;
    let root = directory.path().to_owned();
    let runner = thread::spawn(move || {
        runtime::run(&program, &Registry::default(), &options(&root, 1024 * 1024))
    });

    let both_started = await_file(&first_started) && await_file(&second_started);
    let first_state_path = directory.path().join("first.json");
    let second_state_path = directory.path().join("second.json");
    let snapshots_written = await_file(&second_state_path);
    let first_state = if await_file(&first_state_path) {
        serde_json::from_slice::<Json>(&std::fs::read(&first_state_path)?)?
    } else {
        Json::Null
    };
    let second_state = if snapshots_written {
        serde_json::from_slice::<Json>(&std::fs::read(&second_state_path)?)?
    } else {
        Json::Null
    };
    std::fs::write(&second_gate, b"release")?;
    let result = runner
        .join()
        .map_err(|_| anyhow::anyhow!("background job runner panicked"))??;

    assert!(both_started);
    assert!(snapshots_written);
    assert_eq!(first_state["state"], json!("cancelled"));
    assert_eq!(second_state["state"], json!("running"));
    assert_eq!(result.output, b"second");
    assert_eq!(result.launched_processes, 4);
    Ok(())
}

#[cfg(unix)]
#[test]
fn cancelling_idle_native_job_wakes_its_supervisor() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let started = directory.path().join("idle-job-started");
    let script = "touch \"$1\"; exec sleep 30";
    let wait_for_start = "while [ ! -e \"$1\" ]; do sleep 0.01; done; sleep 0.1";
    let source = format!(
        "spawn as idle\n  run \"sh\" with arguments [\"-c\", {}, \"job\", {}]\nrun \"sh\" with arguments [\"-c\", {}, \"wait\", {}]\ncancel idle\nthen encode \"done\" as json\n",
        quote(script),
        quote(&started.to_string_lossy()),
        quote(wait_for_start),
        quote(&started.to_string_lossy()),
    );
    let program = parser::parse(&source)?;
    let options = options(directory.path(), 1024 * 1024);
    let began = std::time::Instant::now();

    let result = runtime::run(&program, &Registry::default(), &options)?;

    assert!(started.is_file(), "the native background job never started");
    assert_eq!(
        serde_json::from_slice::<Json>(&result.output)?,
        json!("done")
    );
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "cancelling an idle native job waited for its child to exit"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn completed_unobserved_job_still_makes_normal_scope_exit_fail() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse("spawn as finished\n  run \"true\"\n")?;
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )
    .err()
    .expect("unresolved job handles must fail normal exit");
    assert!(format!("{error:#}").contains("awaited or cancelled"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn diagnostic_capture_retains_twenty_kib_without_truncation() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "spawn as diagnostics\n  run \"sh\" with arguments [\"-c\", \"head -c 20000 /dev/zero >&2\"]\nawait diagnostics and keep result as record\nthen encode record as json\n",
    )?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;
    let record: Json = serde_json::from_slice(&result.output)?;
    assert_eq!(record["stderr_limit_per_stage"], json!(65536));
    assert_eq!(record["stderr_truncated"], json!([false]));
    assert_eq!(
        record["stderr_by_stage"][0].as_array().unwrap().len(),
        20000
    );
    assert_eq!(record["stages"][0]["stderr_truncated"], json!(false));
    Ok(())
}

#[cfg(unix)]
#[test]
fn later_invalid_job_stage_is_rejected_before_an_earlier_command_starts() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let sentinel = directory.path().join("must-not-exist");
    let source = format!(
        "spawn as invalid\n  run \"sh\" with arguments [\"-c\", \"touch $1\", \"job\", {}]\n  pipe to \"/missing/execute-job-stage\"\nawait invalid\n",
        quote(&sentinel.to_string_lossy()),
    );
    let program = parser::parse(&source)?;
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )
    .err()
    .expect("invalid pipeline executable must be rejected");
    assert!(!sentinel.exists());
    assert!(!format!("{error:#}").is_empty());
    Ok(())
}

#[cfg(windows)]
#[test]
fn native_windows_fixture_runs_as_an_owned_background_job() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let fixture = Path::new(env!("CARGO_BIN_EXE_native-fixture"));
    let source = format!(
        "spawn as output\n  run {} with arguments [\"write\", \"windows-bytes\"]\nawait output\nthen encode \"done\" as json\n",
        quote(&fixture.to_string_lossy()),
    );
    let program = parser::parse(&source)?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )?;

    assert_eq!(result.launched_processes, 1);
    assert_eq!(
        serde_json::from_slice::<Json>(&result.output)?,
        json!("done")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn sequential_jobs_exceed_the_old_lifetime_limit() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut source = String::new();
    for index in 0..100 {
        source.push_str(&format!(
            "spawn as job{index}\n  run \"true\"\nawait job{index}\n"
        ));
    }
    let program = parser::parse(&source)?;
    runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn retained_records_still_have_a_run_wide_bound() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut source = String::new();
    for index in 0..33 {
        source.push_str(&format!(
            "spawn as job{index}\n  run \"true\"\nawait job{index}\n"
        ));
    }
    for index in 0..33 {
        source.push_str(&format!("inspect job{index} as snapshot{index}\n"));
    }
    let program = parser::parse(&source)?;
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )
    .err()
    .expect("retained record budget must be enforced");
    assert!(format!("{error:#}").contains("retained background job handle limit (32)"));
    Ok(())
}

#[cfg(unix)]
#[test]
#[ignore = "long-running 10,000-process acceptance test"]
fn ten_thousand_sequential_jobs_complete() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut source = String::new();
    for index in 0..10_000 {
        source.push_str(&format!(
            "spawn as job{index}\n  run \"true\"\nawait job{index}\n"
        ));
    }
    let program = parser::parse(&source)?;
    let mut options = options(directory.path(), 1024);
    options.total_timeout = Some(Duration::from_secs(600));
    let result = runtime::run(&program, &Registry::default(), &options)?;
    assert_eq!(result.launched_processes, 10_000);
    Ok(())
}

#[cfg(unix)]
#[test]
fn retained_record_limit_is_shared_with_nested_programs() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("nested-started");
    let mut source = format!(
        "define a program named nested\n  spawn as child\n    run \"touch\" with arguments [{}]\n  await child\n",
        quote(&marker.to_string_lossy())
    );
    for index in 0..32 {
        source.push_str(&format!(
            "spawn as job{index}\n  run \"true\"\nawait job{index}\n"
        ));
    }
    source.push_str("run program nested\n");
    for index in 0..32 {
        source.push_str(&format!("inspect job{index} as snapshot{index}\n"));
    }
    let program = parser::parse(&source)?;
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )
    .err()
    .expect("the nested job must respect the enclosing record budget");
    assert!(format!("{error:#}").contains("retained background job handle limit (32)"));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_calls_keep_same_named_handles_in_their_own_frames() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let source = r#"define a program named inner
  spawn as shared
    run "printf" with arguments ["inner"]
  await shared
  inspect shared as inner_state
  then encode inner_state as json
  then save to "inner.json"
define a program named middle
  spawn as shared
    run "printf" with arguments ["middle"]
  await shared
  run program inner
  inspect shared as middle_state
  then encode middle_state as json
  then save to "middle.json"
  await shared
spawn as shared
  run "printf" with arguments ["root"]
await shared
run program middle
inspect shared as root_state
then encode root_state as json
then save to "root.json"
await shared
"#;
    let program = parser::parse(source)?;
    // Calls execute their inlined bodies in the same step array as the caller.
    assert!(program
        .steps
        .iter()
        .any(|step| matches!(&step.kind, StepKind::CallProgram { .. })));
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;
    assert_eq!(result.output, b"root");
    for name in ["inner", "middle", "root"] {
        let snapshot: Json = serde_json::from_slice(&std::fs::read(
            directory.path().join(format!("{name}.json")),
        )?)?;
        assert_eq!(snapshot["state"], "completed", "{name}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn conditional_job_operations_keep_later_inspection_and_await_available() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    for matched in [true, false] {
        let source = format!(
            r#"spawn as first
  run "printf" with arguments ["first"]
await first
if {matched} is true
  inspect first as yes_state
otherwise
  inspect first as no_state
inspect first as after_state
then encode after_state as json
then save to "state.json"
await first
"#
        );
        let program = parser::parse(&source)?;
        let result = runtime::run(
            &program,
            &Registry::default(),
            &options(directory.path(), 1024 * 1024),
        )?;
        assert_eq!(result.output, b"first", "branch {matched}");
        let snapshot: Json =
            serde_json::from_slice(&std::fs::read(directory.path().join("state.json"))?)?;
        assert_eq!(snapshot["state"], "completed", "branch {matched}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn module_uses_its_own_step_indices_and_preserves_caller_handle() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = directory.path().join("applets");
    write_job_module(&catalog, "test.scope", "unused");
    std::fs::write(
        catalog.join("test_scope.program"),
        r#"define a program named entry
  spawn as shared
    run "printf" with arguments ["module"]
  await shared
  inspect shared as module_state
  then encode module_state as json
  then save to "module.json"
  await shared
  then encode "ok" as json
"#,
    )?;
    let program = parser::parse(
        r#"spawn as shared
  run "printf" with arguments ["caller"]
await shared
run test.scope
inspect shared as caller_state
then encode caller_state as json
then save to "caller.json"
await shared
"#,
    )?;
    let result = runtime::run(
        &program,
        &Registry::load(&catalog)?,
        &options(directory.path(), 1024 * 1024),
    )?;
    assert_eq!(result.output, b"caller");
    for name in ["module", "caller"] {
        let snapshot: Json = serde_json::from_slice(&std::fs::read(
            directory.path().join(format!("{name}.json")),
        )?)?;
        assert_eq!(snapshot["state"], "completed", "{name}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn repeated_reusable_call_uses_fresh_job_scope_at_same_step_indices() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let source = r#"define a program named worker
  spawn as shared
    run "printf" with arguments ["child"]
  await shared
  inspect shared as state
  await shared
repeat 3 times
  run program worker
"#;
    let program = parser::parse(source)?;
    let result = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024 * 1024),
    )?;
    assert_eq!(result.launched_processes, 3);
    assert_eq!(result.output, b"child");
    Ok(())
}

#[cfg(feature = "test-fixtures")]
#[test]
fn diagnostic_capture_limit_and_truncation_are_portable() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let fixture = Path::new(env!("CARGO_BIN_EXE_native-fixture"));
    for (count, retained, truncated) in [
        (20_000, 20_000, false),
        (65_536, 65_536, false),
        (65_537, 65_536, true),
    ] {
        let source = format!(
            "spawn as diagnostics\n  run {} with arguments [\"stderr-bytes\", \"{}\"]\nawait diagnostics and keep result as record\nthen encode record as json\n",
            quote(&fixture.to_string_lossy()),
            count,
        );
        let result = runtime::run(
            &parser::parse(&source)?,
            &Registry::default(),
            &options(directory.path(), 2 * 1024 * 1024),
        )?;
        let record: Json = serde_json::from_slice(&result.output)?;
        let stderr = record["stderr_by_stage"][0].as_array().unwrap();
        assert_eq!(record["stderr_limit_per_stage"], json!(65_536));
        assert_eq!(stderr.len(), retained, "input length {count}");
        assert!(stderr
            .iter()
            .all(|byte| byte.as_u64() == Some(u64::from(b'e'))));
        assert_eq!(record["stderr_truncated"], json!([truncated]));
        assert_eq!(record["stages"][0]["stderr_truncated"], json!(truncated));
    }
    Ok(())
}

#[cfg(feature = "test-fixtures")]
#[test]
fn foreground_result_keeps_twenty_kib_of_diagnostics() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let fixture = Path::new(env!("CARGO_BIN_EXE_native-fixture"));
    let manifest = json!({
        "name": "diagnostic.fixture",
        "binary": fixture.to_string_lossy(),
        "arguments": [{"literal": "stderr-bytes"}, {"literal": "20000"}],
        "stdio": {"stderr": "capture"}
    });
    let registry = Registry::from_sources(&[manifest.to_string()])?;
    let source = "run diagnostic.fixture and keep result as record\nthen encode record as json\n";
    let result = runtime::run(
        &parser::parse(source)?,
        &registry,
        &options(directory.path(), 2 * 1024 * 1024),
    )?;
    let record: Json = serde_json::from_slice(&result.output)?;
    assert_eq!(record["stderr_captured"], json!(true));
    assert_eq!(record["stderr"].as_array().unwrap().len(), 20_000);
    assert_eq!(record["stages"][0]["stderr_bytes"], json!(20_000));

    let mut failing_manifest = manifest;
    failing_manifest["arguments"]
        .as_array_mut()
        .unwrap()
        .push(json!({"literal": "7"}));
    let failing_registry = Registry::from_sources(&[failing_manifest.to_string()])?;
    let failing_source =
        "try\n  run diagnostic.fixture\ncatch failure\n  then encode failure as json\n";
    let failure = runtime::run(
        &parser::parse(failing_source)?,
        &failing_registry,
        &options(directory.path(), 2 * 1024 * 1024),
    )?;
    let caught: Json = serde_json::from_slice(&failure.output)?;
    assert_eq!(caught["stderr_captured"], json!(true));
    assert_eq!(caught["stderr"].as_array().unwrap().len(), 20_000);
    Ok(())
}

#[cfg(feature = "test-fixtures")]
#[test]
fn merged_stderr_preserves_multi_megabyte_output() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let fixture = Path::new(env!("CARGO_BIN_EXE_native-fixture"));
    let source = format!(
        "run {} with arguments [\"stderr-bytes\", \"2097152\"] and stderr merged\n",
        quote(&fixture.to_string_lossy()),
    );
    let result = runtime::run(
        &parser::parse(&source)?,
        &Registry::default(),
        &options(directory.path(), 4 * 1024 * 1024),
    )?;
    assert_eq!(result.output.len(), 2 * 1024 * 1024);
    assert!(result.output.iter().all(|byte| *byte == b'e'));

    let redirected = format!(
        "run {} with arguments [\"stderr-bytes\", \"2097152\"] and stderr to \"diagnostics.bin\"\n",
        quote(&fixture.to_string_lossy()),
    );
    let redirected_result = runtime::run(
        &parser::parse(&redirected)?,
        &Registry::default(),
        &options(directory.path(), 4 * 1024 * 1024),
    )?;
    assert!(redirected_result.output.is_empty());
    let bytes = std::fs::read(directory.path().join("diagnostics.bin"))?;
    assert_eq!(bytes.len(), 2 * 1024 * 1024);
    assert!(bytes.iter().all(|byte| *byte == b'e'));
    Ok(())
}

#[cfg(all(unix, feature = "test-fixtures"))]
#[test]
fn merged_stderr_does_not_wait_for_escaped_descendant_to_close_pipe() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let fixture = Path::new(env!("CARGO_BIN_EXE_native-fixture"));
    let marker = directory.path().join("child-started");
    let release = directory.path().join("release-child");
    let done = directory.path().join("child-started.done");
    let source = format!(
        "run {} with arguments [\"hold-stderr-after-parent-exit\", {}, {}] and stderr merged\n",
        quote(&fixture.to_string_lossy()),
        quote(&marker.to_string_lossy()),
        quote(&release.to_string_lossy()),
    );
    let mut run_options = options(directory.path(), 1024 * 1024);
    run_options.command_timeout = Some(Duration::from_secs(2));
    let started = std::time::Instant::now();
    let result = runtime::run(&parser::parse(&source)?, &Registry::default(), &run_options);
    let elapsed = started.elapsed();
    // Release the escaped descendant even when the result is an error.
    std::fs::write(&release, b"release")?;
    assert!(await_file(&done), "descendant did not exit after release");
    let result = result?;
    assert!(result.output.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "waited {elapsed:?} for the inherited pipe"
    );
    Ok(())
}
