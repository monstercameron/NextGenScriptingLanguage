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
    assert_eq!(record["stderr_limit_per_stage"], json!(8192));
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
fn finite_module_called_from_finally_shares_the_run_handle_budget() -> anyhow::Result<()> {
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
    let error = runtime::run(
        &program,
        &Registry::load(&catalog)?,
        &options(directory.path(), 1024),
    )
    .err()
    .expect("finally's imported module must share the enclosing job budget");
    assert!(format!("{error:#}").contains("job handle limit (32)"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn service_preflight_rejects_jobs_hidden_in_a_finite_module() -> anyhow::Result<()> {
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
    let error = runtime::run_service(
        &service,
        &registry,
        &options(directory.path(), 1024),
        &runtime::ServiceLimits {
            max_runs: Some(1),
            max_record_bytes: 1024,
        },
        |_| Ok(()),
    )
    .err()
    .expect("a service must reject job operations imported through a finite module");
    assert!(format!("{error:#}").contains("including imported program modules"));
    assert!(
        !effect.exists(),
        "rejection must precede the nested job effect"
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
fn nested_finite_program_shares_the_created_handle_cap() -> anyhow::Result<()> {
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
    let error = runtime::run(
        &program,
        &Registry::default(),
        &options(directory.path(), 1024),
    )
    .err()
    .expect("nested call must share the root run's created-handle cap");
    assert!(format!("{error:#}").contains("job handle limit (32)"));
    assert!(
        !child_effect.exists(),
        "a nested job beyond the handle limit must not start"
    );
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
fn diagnostic_capture_reports_the_exact_unix_limit_and_real_truncation() -> anyhow::Result<()> {
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
    assert_eq!(record["stderr_limit_per_stage"], json!(8192));
    assert_eq!(record["stderr_truncated"], json!([true]));
    assert_eq!(record["stderr_by_stage"][0].as_array().unwrap().len(), 8192);
    assert_eq!(record["stages"][0]["stderr_truncated"], json!(true));
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
