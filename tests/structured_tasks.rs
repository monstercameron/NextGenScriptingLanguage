use anyhow::{anyhow, Result};
use flow::tasks::TaskScope;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn scope_enforces_bound_and_preserves_typed_join_results() -> Result<()> {
    let mut scope = TaskScope::new(2, None)?;
    let start = Arc::new(Barrier::new(3));
    let release = Arc::new(Barrier::new(3));

    let mut handles = Vec::new();
    for value in [17_u32, 29_u32] {
        let start = Arc::clone(&start);
        let release = Arc::clone(&release);
        handles.push(scope.spawn(move |token| -> Result<(u32, bool)> {
            start.wait();
            let not_cancelled = token.check().is_ok();
            release.wait();
            Ok((value, not_cancelled))
        })?);
    }

    // Both workers rendezvous with the test while they are still active, so
    // the assertions below do not depend on a sleep or scheduler timing.
    start.wait();
    let active_while_held = scope.active_count();
    let not_ready_while_held = scope.try_join(&mut handles[0]);
    let extra_spawn = scope.spawn(|_| Ok::<_, anyhow::Error>(41_u32));
    let extra_was_rejected = extra_spawn.is_err();

    release.wait();
    let first = scope.join(handles.remove(0))?;
    let second = scope.join(handles.remove(0))?;
    let extra_result = match extra_spawn {
        Ok(handle) => Some(scope.join(handle)?),
        Err(_) => None,
    };

    assert_eq!(scope.max_parallel(), 2);
    assert_eq!(active_while_held, 2);
    assert!(matches!(not_ready_while_held, Ok(None)));
    assert!(
        extra_was_rejected,
        "spawn at capacity should fail immediately"
    );
    assert_eq!(extra_result, None);
    assert_eq!(first, (17, true));
    assert_eq!(second, (29, true));
    assert_eq!(scope.active_count(), 0);
    Ok(())
}

#[test]
fn cancellation_and_cancel_and_join_stop_a_cooperative_worker() -> Result<()> {
    let mut scope = TaskScope::new(1, None)?;
    let started = Arc::new(Barrier::new(2));
    let worker_started = Arc::clone(&started);
    let handle = scope.spawn(move |token| -> Result<bool> {
        worker_started.wait();
        while !token.is_cancelled() {
            thread::yield_now();
        }
        Ok(token.is_cancelled())
    })?;

    started.wait();
    scope.cancel();
    scope.cancel_and_join()?;

    assert!(scope.join(handle)?);
    assert_eq!(scope.active_count(), 0);
    Ok(())
}

#[test]
fn dropping_scope_cancels_and_joins_children() -> Result<()> {
    let returned = Arc::new(AtomicBool::new(false));
    let started = Arc::new(Barrier::new(2));
    let (cancelled_tx, cancelled_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (drop_finished_tx, drop_finished_rx) = mpsc::channel();

    let mut scope = TaskScope::new(1, None)?;
    let worker_returned = Arc::clone(&returned);
    let worker_started = Arc::clone(&started);
    let _handle = scope.spawn(move |token| -> Result<()> {
        worker_started.wait();
        while !token.is_cancelled() {
            thread::yield_now();
        }
        let _ = cancelled_tx.send(());
        release_rx.recv()?;
        worker_returned.store(true, Ordering::Release);
        Ok(())
    })?;
    started.wait();

    let dropper = thread::spawn(move || {
        drop(scope);
        let _ = drop_finished_tx.send(());
    });
    let cancellation_observed = cancelled_rx.recv().is_ok();
    // The child is now known to be blocked on release_rx. A completed drop
    // here would mean the scope returned without joining that child.
    let drop_finished_while_child_held = drop_finished_rx.try_recv().is_ok();
    let released = release_tx.send(()).is_ok();
    let dropper_joined = dropper.join().is_ok();

    assert!(cancellation_observed);
    assert!(!drop_finished_while_child_held);
    assert!(released);
    assert!(dropper_joined);
    assert!(returned.load(Ordering::Acquire));
    Ok(())
}

#[test]
fn join_propagates_task_errors_and_panics() -> Result<()> {
    let mut scope = TaskScope::new(2, None)?;
    let error_task = scope.spawn(|_| Err::<(), _>(anyhow!("expected task failure")))?;
    let panic_task = scope.spawn(|_| -> Result<()> { panic!("expected task panic") })?;

    let task_error = scope.join(error_task).unwrap_err();
    let panic_error = scope.join(panic_task).unwrap_err();
    assert!(format!("{task_error:#}").contains("expected task failure"));
    assert!(format!("{panic_error:#}").contains("expected task panic"));
    Ok(())
}

#[test]
fn expired_deadline_is_visible_and_prevents_new_work() -> Result<()> {
    let deadline = Instant::now() - Duration::from_secs(1);
    let mut scope = TaskScope::new(1, Some(deadline))?;
    let token = scope.cancellation_token();

    assert_eq!(token.deadline(), Some(deadline));
    assert!(token.is_cancelled());
    assert!(token.check().unwrap_err().to_string().contains("deadline"));
    assert!(scope.spawn(|_| Ok::<_, anyhow::Error>(())).is_err());
    Ok(())
}

#[test]
fn zero_parallelism_is_rejected() {
    assert!(TaskScope::new(0, None).is_err());
}

#[test]
fn parallel_program_runs_through_the_cli_frontend_and_keeps_declared_order() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let example = root.join("examples/tasks/parallel.program");
    let source = std::fs::read_to_string(&example).unwrap();
    let program = flow::parser::parse(&source).unwrap();
    assert!(matches!(
        program.steps.first().map(|step| &step.kind),
        Some(flow::ast::StepKind::TaskGroup {
            mode: flow::ast::TaskGroupMode::Join,
            limit: 2,
            binding,
            tasks,
            each: None,
        }) if binding == "results"
            && tasks.iter().map(|task| task.binding.as_deref()).collect::<Vec<_>>()
                == vec![Some("sum"), Some("product")]
    ));

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&example)
        .current_dir(root)
        .output()
        .expect("start execute CLI");
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["mode"], "join");
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["id"], "sum");
    assert_eq!(tasks[0]["state"], "success");
    assert_eq!(tasks[0]["value"], 42);
    assert_eq!(tasks[1]["id"], "product");
    assert_eq!(tasks[1]["state"], "success");
    assert_eq!(tasks[1]["value"], 42);
}

#[test]
fn race_program_reports_a_successful_winner_through_the_cli_frontend() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let example = root.join("examples/tasks/race.program");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&example)
        .current_dir(root)
        .output()
        .expect("start execute CLI");
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["mode"], "race");
    assert!(matches!(
        result["winner"].as_str(),
        Some("quotient" | "sum")
    ));
    assert!(result["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|task| { task["id"] == result["winner"] && task["state"] == "success" }));
}

#[test]
fn parallel_for_each_runs_every_element_and_keeps_failures_as_records() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let example = root.join("examples/tasks/for-each.program");
    let source = std::fs::read_to_string(&example).unwrap();
    let program = flow::parser::parse(&source).unwrap();
    assert!(matches!(
        program.steps.first().map(|step| &step.kind),
        Some(flow::ast::StepKind::TaskGroup {
            limit: 2,
            binding,
            tasks,
            each: Some(each),
            ..
        }) if binding == "results" && tasks.len() == 1 && each.binding == "divisor"
    ));

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&example)
        .current_dir(root)
        .output()
        .expect("start execute CLI");
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["mode"], "for_each");
    assert_eq!(result["succeeded"], 2);
    assert_eq!(result["failed"], 1);
    assert_eq!(result["cancelled"], 0);
    assert_eq!(result["not_started"], 0);
    let items = result["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    for (index, item) in items.iter().enumerate() {
        assert_eq!(item["index"], index);
    }
    assert_eq!(items[0]["item"], 6);
    assert_eq!(items[0]["state"], "success");
    assert_eq!(items[0]["value"], 2.0);
    // Division by zero is that element's outcome; its siblings still ran.
    assert_eq!(items[1]["item"], 0);
    assert_eq!(items[1]["state"], "non_success");
    assert_eq!(items[2]["item"], 3);
    assert_eq!(items[2]["value"], 4.0);
}

#[cfg(unix)]
#[test]
fn parallel_for_each_failure_identifies_compact_partial_result_schema() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = "parallel for each n in [\"a\", \"b\"] at most 1 as results\n  run \"sh\" with arguments [\"-c\", \"if [ $0 = a ]; then printf xx; else head -c 2000 /dev/zero; fi\", n]\n";
    let program = flow::parser::parse(source)?;
    let registry = flow::manifest::Registry::load(&root.join("manifests"))?;
    flow::runtime::validate(&program, &registry)?;
    let options = flow::runtime::RunOptions {
        max_output_bytes: 1500,
        command_timeout: Some(Duration::from_secs(8)),
        ..flow::runtime::RunOptions::new(root.to_path_buf())
    };
    let error = match flow::runtime::run(&program, &registry, &options) {
        Err(error) => error,
        Ok(_) => {
            return Err(anyhow!(
                "second element did not exceed the shared raw output limit"
            ));
        }
    };
    let failure = error
        .downcast_ref::<flow::runtime::tasks::TaskGroupFailure>()
        .expect("task-group failure keeps typed partial results");
    assert_eq!(failure.partial_results_schema_version, Some(3));
    let flow::ast::Value::List(records) = &failure.partial_results else {
        panic!("partial results should remain a list")
    };
    let flow::ast::Value::Record(first) = &records[0] else {
        panic!("first partial result should be a record")
    };
    assert_eq!(
        first.get("output_encoding"),
        Some(&flow::ast::Value::String("utf8".into()))
    );
    assert_eq!(
        first.get("output"),
        Some(&flow::ast::Value::String("xx".into()))
    );
    Ok(())
}

#[test]
fn parallel_for_each_of_an_empty_list_returns_empty_records() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = "parallel for each word in [] at most 4 as results\n  run text.concat with parts [word, \"!\"] and separator \"\"\nthen encode results as json\n";
    let program = flow::parser::parse(source)?;
    let registry = flow::manifest::Registry::load(&root.join("manifests"))?;
    flow::runtime::validate(&program, &registry)?;
    let options = flow::runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(8)),
        ..flow::runtime::RunOptions::new(root.to_path_buf())
    };
    let result = flow::runtime::run(&program, &registry, &options)?;
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    assert_eq!(value["items"], serde_json::json!([]));
    assert_eq!(value["succeeded"], 0);
    assert_eq!(result.launched_processes, 0);

    // Empty loop items are uninhabited, so every valid consumer type remains
    // accepted while the loop body is still structurally validated.
    let json_consumer = flow::parser::parse(
        "parallel for each item in [] at most 4 as results\n  run json.get with document item and pointer \"\"\n",
    )?;
    flow::runtime::validate(&json_consumer, &registry)?;
    let integer_consumer = flow::parser::parse(
        "parallel for each item in [] at most 4 as results\n  run color.rgb_to_hex with red item and green 0 and blue 0\n",
    )?;
    flow::runtime::validate(&integer_consumer, &registry)?;
    let list_consumer = flow::parser::parse(
        "parallel for each item in [] at most 4 as results\n  run list.chunk with items [item] and size 1\n",
    )?;
    flow::runtime::validate(&list_consumer, &registry)?;
    let nested_empty_consumer = flow::parser::parse(
        "for each item in []\n  for each nested in item\n    run color.rgb_to_hex with red nested and green 0 and blue 0\n",
    )?;
    flow::runtime::validate(&nested_empty_consumer, &registry)?;
    Ok(())
}

#[test]
fn parallel_for_each_rejects_invalid_shapes_before_execution() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry = flow::manifest::Registry::load(&root.join("manifests"))?;
    let header =
        "accept words as list of text\nparallel for each word in words at most 2 as results\n";
    for (body, expected) in [
        (
            "  run \"echo\" with arguments [word] as out\n",
            "cannot use `as`",
        ),
        (
            "  run \"echo\" with arguments [word]\n  run \"echo\" with arguments [word]\n",
            "exactly one `run`",
        ),
        (
            "  then \"echo\" with arguments [word]\n",
            "one `run` command",
        ),
    ] {
        let error = flow::parser::parse(&format!("{header}{body}")).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    let error = flow::parser::parse(
        "parallel for each word in [\"a\"] at most 65 as results\n  run \"echo\" with arguments [word]\n",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("between 1 and 64"),
        "{error:#}"
    );

    let undefined = flow::parser::parse(&format!(
        "{header}  run \"echo\" with arguments [missing]\n"
    ))?;
    let error = flow::runtime::validate(&undefined, &registry).unwrap_err();
    assert!(format!("{error:#}").contains("missing"), "{error:#}");

    let shadowing = flow::parser::parse(
        "accept word as text\naccept words as list of text\nparallel for each word in words at most 2 as results\n  run \"echo\" with arguments [word]\n",
    )?;
    let error = flow::runtime::validate(&shadowing, &registry).unwrap_err();
    assert!(
        format!("{error:#}").contains("already defined"),
        "{error:#}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn parallel_for_each_bounds_concurrent_direct_executables() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let worker = directory.path().join("worker.sh");
    // Each element holds a marker while it runs and records how many markers
    // it observed; the bound holds if no element ever sees more than two.
    write_executable(
        &worker,
        "#!/bin/sh\nmkdir \"active.$1\"\nls -d active.* | wc -l | tr -d ' ' >> seen\nsleep 0.2\nrmdir \"active.$1\"\nprintf 'done %s\\n' \"$1\"\n",
    )?;
    let source = format!(
        "parallel for each n in [\"1\", \"2\", \"3\", \"4\", \"5\", \"6\"] at most 2 as results\n  run {} with arguments [n]\nthen encode results as json\n",
        quote_path(&worker)?
    );
    let manifests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let result = run_program(&source, &manifests, directory.path())?;
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    assert_eq!(value["succeeded"], 6);
    assert_eq!(value["items"][3]["output_encoding"], "utf8");
    assert_eq!(value["items"][3]["output"], "done 4\n");
    assert_eq!(result.launched_processes, 6);
    let seen = std::fs::read_to_string(directory.path().join("seen"))?;
    let peak = seen
        .lines()
        .map(|line| line.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max();
    assert!(peak.is_some_and(|peak| peak <= 2), "observed {seen:?}");
    Ok(())
}

#[cfg(unix)]
#[test]
fn parallel_for_each_preserves_input_order_when_completion_order_differs() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let worker = directory.path().join("staggered.sh");
    write_executable(
        &worker,
        "#!/bin/sh\ncase \"$1\" in 1) sleep 0.15;; 2) sleep 0.08;; 3) sleep 0.01;; esac\nprintf 'done %s\\n' \"$1\"\n",
    )?;
    let source = format!(
        "parallel for each n in [\"1\", \"2\", \"3\"] at most 3 as results\n  run {} with arguments [n]\nthen encode results as json\n",
        quote_path(&worker)?
    );
    let manifests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let result = run_program(&source, &manifests, directory.path())?;
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;

    assert_eq!(value["items"][0]["output"], "done 1\n");
    assert_eq!(value["items"][1]["output"], "done 2\n");
    assert_eq!(value["items"][2]["output"], "done 3\n");
    Ok(())
}

#[test]
fn independent_task_groups_reject_sibling_binding_reads() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = "parallel at most 2 as result\n  run number.add with left 1 and right 2 as first\n  run number.add with left first and right 3 as second\n";
    let program = flow::parser::parse(source)?;
    let registry = flow::manifest::Registry::load(&root.join("manifests"))?;
    let error = flow::runtime::validate(&program, &registry).unwrap_err();
    assert!(format!("{error:#}").contains("first"));
    Ok(())
}

#[test]
fn repeated_number_add_public_workload_has_exact_output() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = flow::parser::parse(include_str!(
        "../benchmarks/workloads/resolved-json-arguments-repeat-1000.program"
    ))?;
    let registry = flow::manifest::Registry::load(&root.join("manifests"))?;
    let cwd = tempfile::tempdir()?;
    let options = flow::runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: Some(Duration::from_secs(10)),
        ..flow::runtime::RunOptions::new(cwd.path().to_path_buf())
    };

    let result = flow::runtime::run(&program, &registry, &options)?;
    assert_eq!(result.output, b"3");
    assert_eq!(result.launched_processes, 0);
    assert_eq!(result.executions.len() + result.dropped_executions, 1000);
    Ok(())
}

#[cfg(unix)]
#[test]
fn race_cancels_and_reaps_a_losing_native_process() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let manifests = temp.path().join("manifests");
    std::fs::create_dir(&manifests)?;
    let started = temp.path().join("slow-started");
    let completed = temp.path().join("slow-completed");
    let slow = temp.path().join("slow.sh");
    let fast = temp.path().join("fast.sh");
    write_executable(
        &slow,
        "#!/bin/sh\nprintf started > \"$1\"\nsleep 5 | cat >/dev/null\nprintf completed > \"$2\"\nprintf slow\n",
    )?;
    write_executable(
        &fast,
        "#!/bin/sh\nwhile [ ! -f \"$1\" ]; do sleep 0.01; done\nprintf fast\n",
    )?;
    write_manifest(
        &manifests,
        "fixture.slow",
        &slow,
        &["started", "completed"],
        false,
    )?;
    write_manifest(&manifests, "fixture.fast", &fast, &["started"], false)?;

    let source = format!(
        "race at most 2 as result\n  run fixture.slow with started {} and completed {} as slow\n  run fixture.fast with started {} as fast\nthen encode result as json\n",
        quote_path(&started)?,
        quote_path(&completed)?,
        quote_path(&started)?,
    );
    let result = run_program(&source, &manifests, temp.path())?;
    assert_eq!(result.launched_processes, 2);
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    assert!(matches!(value["winner"].as_str(), Some("slow" | "fast")));
    assert!(started.is_file(), "barrier proves the slow process started");
    assert!(
        !completed.exists(),
        "the losing process must be canceled and reaped before the race returns"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn join_failure_cancels_siblings_and_keeps_failure_uncertainty() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let manifests = temp.path().join("manifests");
    std::fs::create_dir(&manifests)?;
    let sibling_started = temp.path().join("sibling-started");
    let sibling_completed = temp.path().join("sibling-completed");
    let fail = temp.path().join("fail.sh");
    let sibling = temp.path().join("sibling.sh");
    write_executable(
        &fail,
        "#!/bin/sh\nwhile [ ! -f \"$1\" ]; do sleep 0.01; done\nprintf failure >&2\nexit 9\n",
    )?;
    write_executable(
        &sibling,
        "#!/bin/sh\nprintf started > \"$1\"\nsleep 5 | cat >/dev/null\nprintf completed > \"$2\"\nprintf sibling\n",
    )?;
    write_manifest(&manifests, "fixture.fail", &fail, &["started"], true)?;
    write_manifest(
        &manifests,
        "fixture.sibling",
        &sibling,
        &["started", "completed"],
        false,
    )?;

    let source = format!(
        "parallel at most 2 as result\n  run fixture.fail with started {} as failing\n  run fixture.sibling with started {} and completed {} as sibling\n",
        quote_path(&sibling_started)?,
        quote_path(&sibling_started)?,
        quote_path(&sibling_completed)?,
    );
    let error = run_program(&source, &manifests, temp.path())
        .err()
        .expect("task-group failure should be returned as an error");
    let failure = error
        .downcast_ref::<flow::runtime::tasks::TaskGroupFailure>()
        .expect("task group failures keep their typed payload");
    assert_eq!(failure.launched_processes, 2);
    let message = format!("{error:#}");
    assert!(
        message.contains("task group failed at task \"failing\""),
        "{message}"
    );
    assert!(message.contains("uncertain effects: true"), "{message}");
    assert!(
        sibling_started.is_file(),
        "barrier proves the sibling started"
    );
    assert!(
        !sibling_completed.exists(),
        "a failed join must cancel and reap active siblings"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn join_failure_aggregates_uncertainty_from_cancelled_sibling_in_either_order() -> Result<()> {
    for known_failure_first in [true, false] {
        let temp = tempfile::tempdir()?;
        let manifests = temp.path().join("manifests");
        std::fs::create_dir(&manifests)?;
        let sibling_started = temp.path().join("sibling-started");
        let sibling_effect = temp.path().join("sibling-effect");
        let sibling_completed = temp.path().join("sibling-completed");
        let fail = temp.path().join("known-failure.sh");
        let sibling = temp.path().join("uncertain-sibling.sh");
        write_executable(
            &fail,
            "#!/bin/sh\nwhile [ ! -f \"$1\" ]; do sleep 0.01; done\nexit 9\n",
        )?;
        write_executable(
            &sibling,
            "#!/bin/sh\nprintf started > \"$1\"\nprintf effect > \"$2\"\nsleep 5 | cat >/dev/null\nprintf completed > \"$3\"\n",
        )?;
        write_manifest(&manifests, "fixture.known_fail", &fail, &["effect"], false)?;
        let fail_manifest_path = manifests.join("fixture_known_fail.json");
        let mut fail_manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&fail_manifest_path)?)?;
        fail_manifest["contract"]["outcomes"] = serde_json::json!([
            {"status": 0, "name": "success", "kind": "success"},
            {
                "status": 9,
                "name": "known_failure",
                "kind": "failure",
                "effect_certainty": "known_response"
            }
        ]);
        std::fs::write(
            &fail_manifest_path,
            serde_json::to_vec_pretty(&fail_manifest)?,
        )?;
        write_manifest(
            &manifests,
            "fixture.uncertain_sibling",
            &sibling,
            &["started", "effect", "completed"],
            true,
        )?;

        let known_failure_call = format!(
            "run fixture.known_fail with effect {} as known",
            quote_path(&sibling_effect)?
        );
        let sibling_call = format!(
            "run fixture.uncertain_sibling with started {} and effect {} and completed {} as sibling",
            quote_path(&sibling_started)?,
            quote_path(&sibling_effect)?,
            quote_path(&sibling_completed)?,
        );
        let (first, second) = if known_failure_first {
            (known_failure_call, sibling_call)
        } else {
            (sibling_call, known_failure_call)
        };
        let source = format!("parallel at most 2 as result\n  {first}\n  {second}\n");
        let error = run_program(&source, &manifests, temp.path())
            .err()
            .expect("known non-success outcome should fail the joined group");
        let failure = error
            .downcast_ref::<flow::runtime::tasks::TaskGroupFailure>()
            .expect("task-group error keeps its typed payload");
        assert_eq!(failure.failed_task, "known");
        assert!(
            sibling_effect.is_file(),
            "sibling effect barrier was reached"
        );
        assert!(
            !sibling_completed.exists(),
            "sibling was canceled and reaped"
        );
        assert!(
            failure.uncertain,
            "canceled sibling uncertainty must be reflected in the aggregate for known_failure_first={known_failure_first}"
        );
        let flow::ast::Value::List(records) = &failure.partial_results else {
            panic!("partial task results should remain a list")
        };
        let sibling_record = records.iter().find_map(|record| {
            let flow::ast::Value::Record(fields) = record else {
                return None;
            };
            matches!(fields.get("id"), Some(flow::ast::Value::String(id)) if id == "sibling")
                .then_some(fields)
        });
        let sibling_record = sibling_record.expect("partial results include the sibling");
        assert_eq!(
            sibling_record.get("uncertain"),
            Some(&flow::ast::Value::Bool(true))
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn task_group_preflights_every_applet_before_any_group_effect() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let manifests = temp.path().join("manifests");
    std::fs::create_dir(&manifests)?;
    let marker = temp.path().join("marker");
    let script = temp.path().join("effect.sh");
    write_executable(&script, "#!/bin/sh\nprintf effect > \"$1\"\nprintf done\n")?;
    write_manifest(&manifests, "fixture.effect", &script, &["marker"], false)?;

    let source = format!(
        "parallel at most 2 as result\n  run fixture.effect with marker {} as first\n  run missing.applet as second\n",
        quote_path(&marker)?,
    );
    let error = run_program(&source, &manifests, temp.path())
        .err()
        .expect("invalid later task should fail preflight");
    assert!(format!("{error:#}").contains("missing.applet"));
    assert!(
        !marker.exists(),
        "invalid later tasks must fail preflight before the earlier task runs"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn task_group_starts_only_the_configured_number_of_workers_before_the_barrier() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();
    let manifests = root.join("manifests");
    std::fs::create_dir(&manifests)?;
    let gate = root.join("release");
    let worker = root.join("barrier.sh");
    write_executable(
        &worker,
        "#!/bin/sh\nprintf ready > \"$1\"\nwhile [ ! -f \"$2\" ]; do sleep 0.01; done\nprintf ready\n",
    )?;
    write_manifest(
        &manifests,
        "fixture.barrier",
        &worker,
        &["started", "gate"],
        false,
    )?;

    let started = [
        root.join("one.started"),
        root.join("two.started"),
        root.join("three.started"),
    ];
    let source = format!(
        "parallel at most 2 as result\n  run fixture.barrier with started {} and gate {} as one\n  run fixture.barrier with started {} and gate {} as two\n  run fixture.barrier with started {} and gate {} as three\nthen encode result as json\n",
        quote_path(&started[0])?,
        quote_path(&gate)?,
        quote_path(&started[1])?,
        quote_path(&gate)?,
        quote_path(&started[2])?,
        quote_path(&gate)?,
    );
    let manifest_path = manifests.clone();
    let runner = thread::spawn(move || {
        let program = flow::parser::parse(&source)?;
        let registry = flow::manifest::Registry::load(&manifest_path)?;
        let options = flow::runtime::RunOptions {
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(8)),
            ..flow::runtime::RunOptions::new(root)
        };
        flow::runtime::run(&program, &registry, &options)
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while !(started[0].is_file() && started[1].is_file()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let first_two_reached_barrier = started[0].is_file() && started[1].is_file();
    let third_was_pending = !started[2].exists();
    std::fs::write(&gate, b"release")?;
    let result = runner
        .join()
        .map_err(|_| anyhow!("task group runner panicked"))??;
    assert!(
        first_two_reached_barrier,
        "the first two tasks must reach the barrier"
    );
    assert!(
        third_was_pending,
        "the third task must remain pending while both bounded slots are held"
    );
    assert!(started[2].is_file());
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    let tasks = value["tasks"].as_array().unwrap();
    assert_eq!(
        tasks
            .iter()
            .map(|task| task["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["one", "two", "three"]
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn task_group_json_redacts_declared_secret_stderr_but_api_keeps_raw_bytes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let secret = b"task-json-secret-sentinel";
    let source = serde_json::json!({
        "name": "secret.echo",
        "binary": "/bin/sh",
        "arguments": [
            {"literal": "-c"},
            {"literal": "printf '%s' \"$FLOW_SENTINEL\" >&2; printf output"}
        ],
        "stdio": {"stderr": "capture"},
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"},
            "environment": {"set": {"FLOW_SENTINEL": std::str::from_utf8(secret)?}}
        }
    });
    let make_registry = || -> Result<flow::manifest::Registry> {
        let registry = flow::manifest::Registry::from_sources(&[source.to_string()])?;
        registry.set_host_state(flow::security::DataFlowPolicy::from_json(
            br#"{"version":1,"secrets":{"environment":{"secret.echo":["FLOW_SENTINEL"]}}}"#,
        )?);
        Ok(registry)
    };
    let options = flow::runtime::RunOptions {
        max_output_bytes: 4096,
        command_timeout: Some(Duration::from_secs(5)),
        ..flow::runtime::RunOptions::new(temp.path().to_path_buf())
    };

    // The backend/API keeps exact stderr bytes; only task-record presentation
    // applies the registry redactor.
    let api_result = flow::runtime::invoke_applet(
        &make_registry()?,
        "secret.echo",
        Default::default(),
        Vec::new(),
        &options,
        Arc::new(AtomicBool::new(false)),
    )?;
    assert_eq!(api_result.stderr, secret);

    let registry = make_registry()?;
    let program = flow::parser::parse(
        "parallel at most 1 as results\n  run secret.echo as echoed\nthen encode results as json\n",
    )?;
    let result = flow::runtime::run(&program, &registry, &options)?;
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    let stderr = value["tasks"][0]["stderr"]
        .as_array()
        .ok_or_else(|| anyhow!("task stderr should be encoded as a byte array"))?;
    let stderr_bytes = stderr
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|byte| u8::try_from(byte).ok())
                .ok_or_else(|| anyhow!("task stderr byte is outside u8 range"))
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(stderr_bytes, b"[REDACTED]");
    let raw_secret_numbers = secret
        .iter()
        .map(|byte| u64::from(*byte))
        .collect::<Vec<_>>();
    assert_ne!(
        value["tasks"][0]["stderr"],
        serde_json::json!(raw_secret_numbers)
    );
    assert!(!result
        .output
        .windows(secret.len())
        .any(|window| window == secret));

    // V3 element records redact before encoding, while leaving the backend's
    // raw API unchanged.
    let program = flow::parser::parse(
        "parallel for each item in [\"one\"] at most 1 as results\n  run secret.echo\nthen encode results as json\n",
    )?;
    let result = flow::runtime::run(&program, &make_registry()?, &options)?;
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    let element = &value["items"][0];
    assert_eq!(element["stderr_encoding"], "utf8");
    assert_eq!(element["stderr"], "[REDACTED]");
    assert!(!result
        .output
        .windows(secret.len())
        .any(|window| window == secret));
    Ok(())
}

#[cfg(unix)]
#[test]
fn task_group_json_redacts_declared_secret_stdout_but_api_keeps_raw_bytes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let secret = b"task-json-stdout-secret-sentinel";
    let source = serde_json::json!({
        "name": "secret.echo",
        "binary": "/bin/sh",
        "arguments": [
            {"literal": "-c"},
            {"literal": "printf '%s' \"$FLOW_SENTINEL\"; printf '%s' \"$FLOW_SENTINEL\" >&2"}
        ],
        "stdio": {"stderr": "capture"},
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"},
            "environment": {"set": {"FLOW_SENTINEL": std::str::from_utf8(secret)?}}
        }
    });
    let make_registry = || -> Result<flow::manifest::Registry> {
        let registry = flow::manifest::Registry::from_sources(&[source.to_string()])?;
        registry.set_host_state(flow::security::DataFlowPolicy::from_json(
            br#"{"version":1,"secrets":{"environment":{"secret.echo":["FLOW_SENTINEL"]}}}"#,
        )?);
        Ok(registry)
    };
    let options = flow::runtime::RunOptions {
        max_output_bytes: 4096,
        command_timeout: Some(Duration::from_secs(5)),
        ..flow::runtime::RunOptions::new(temp.path().to_path_buf())
    };

    // The backend/API keeps exact stdout bytes; only task-record presentation
    // is redacted.
    let api_result = flow::runtime::invoke_applet(
        &make_registry()?,
        "secret.echo",
        Default::default(),
        Vec::new(),
        &options,
        Arc::new(AtomicBool::new(false)),
    )?;
    assert_eq!(api_result.output, secret);

    let registry = make_registry()?;
    let program = flow::parser::parse(
        "parallel at most 1 as results\n  run secret.echo as echoed\nthen encode results as json\n",
    )?;
    let result = flow::runtime::run(&program, &registry, &options)?;
    assert!(!result
        .output
        .windows(secret.len())
        .any(|window| window == secret));
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    let task = &value["tasks"][0];
    assert_eq!(task["output"], serde_json::json!([91, 82, 69, 68, 65, 67, 84, 69, 68, 93]));
    let stderr = task["stderr"]
        .as_array()
        .ok_or_else(|| anyhow!("task stderr should be encoded as a byte array"))?;
    let stderr_bytes = stderr
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|byte| u8::try_from(byte).ok())
                .ok_or_else(|| anyhow!("task stderr byte is outside u8 range"))
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(stderr_bytes, b"[REDACTED]");

    // V3 element records redact stdout and stderr before encoding, while
    // leaving the backend's raw API unchanged.
    let program = flow::parser::parse(
        "parallel for each item in [\"one\"] at most 1 as results\n  run secret.echo\nthen encode results as json\n",
    )?;
    let result = flow::runtime::run(&program, &make_registry()?, &options)?;
    assert!(!result
        .output
        .windows(secret.len())
        .any(|window| window == secret));
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    let element = &value["items"][0];
    assert_eq!(element["output"], "[REDACTED]");
    assert_eq!(element["stderr"], "[REDACTED]");
    Ok(())
}

#[cfg(unix)]
#[test]
fn parallel_each_bounds_one_byte_secret_redaction_expansion() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = serde_json::json!({
        "name": "secret.repeat",
        "binary": "/bin/sh",
        "arguments": [
            {"literal": "-c"},
            {"literal": "echo completed >> ledger; head -c 3500 /dev/zero | tr '\\000' \"$FLOW_SENTINEL\" >&2; printf output"}
        ],
        "stdio": {"stderr": "capture"},
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"},
            "environment": {"set": {"FLOW_SENTINEL": "x"}}
        }
    });
    let registry = flow::manifest::Registry::from_sources(&[source.to_string()])?;
    registry.set_host_state(flow::security::DataFlowPolicy::from_json(
        br#"{"version":1,"secrets":{"environment":{"secret.repeat":["FLOW_SENTINEL"]}}}"#,
    )?);
    let program = flow::parser::parse(
        "parallel for each item in [\"one\"] at most 1 as results\n  run secret.repeat\nuse value \"done\"\n",
    )?;
    let options = flow::runtime::RunOptions {
        max_output_bytes: 4096,
        command_timeout: Some(Duration::from_secs(5)),
        ..flow::runtime::RunOptions::new(temp.path().to_path_buf())
    };
    let result = flow::runtime::run(&program, &registry, &options)?;
    assert_eq!(result.output, b"done");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("ledger"))?,
        "completed\n"
    );
    Ok(())
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents)?;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(unix)]
fn write_manifest(
    directory: &std::path::Path,
    name: &str,
    binary: &std::path::Path,
    parameters: &[&str],
    uncertain_on_failure: bool,
) -> Result<()> {
    let properties = parameters
        .iter()
        .map(|parameter| {
            (
                (*parameter).to_owned(),
                serde_json::json!({"type":"string","required":true}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let arguments = parameters
        .iter()
        .map(|parameter| serde_json::json!({"parameter":parameter}))
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "name": name,
        "binary": binary.display().to_string(),
        "uncertain_on_failure": uncertain_on_failure,
        "parameters": properties,
        "arguments": arguments,
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind":"oneshot"},
            "output": {"type":"text","encoding":"raw"},
            "capabilities": [],
            "effects": []
        }
    });
    std::fs::write(
        directory.join(format!("{}.json", name.replace('.', "_"))),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

#[cfg(unix)]
fn quote_path(path: &std::path::Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str()
            .ok_or_else(|| anyhow!("fixture path is not UTF-8"))?,
    )?)
}

#[cfg(unix)]
fn run_program(
    source: &str,
    manifests: &std::path::Path,
    working_directory: &std::path::Path,
) -> Result<flow::runtime::RunResult> {
    let program = flow::parser::parse(source)?;
    let registry = flow::manifest::Registry::load(manifests)?;
    let options = flow::runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(8)),
        ..flow::runtime::RunOptions::new(working_directory.to_path_buf())
    };
    flow::runtime::run(&program, &registry, &options)
}
