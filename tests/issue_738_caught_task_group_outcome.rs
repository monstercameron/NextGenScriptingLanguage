use anyhow::Result;
use std::time::Duration;

/// Materialize the bundled manifests in `manifest_root`. The local `manifests/`
/// folder is avoided because its Windows ACLs fail the trust gate here.
fn bundled_registry(manifest_root: &std::path::Path) -> Result<flow::manifest::Registry> {
    let index = flow::bundled_manifests::index()?;
    for (position, entry) in index.entries().iter().enumerate() {
        std::fs::write(manifest_root.join(entry.file()), index.body(position)?)?;
    }
    flow::manifest::Registry::from_bundled(manifest_root)
}

fn run_caught(source: &str) -> Result<serde_json::Value> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = flow::parser::parse(source)?;
    let manifest_root = tempfile::tempdir()?;
    let registry = bundled_registry(manifest_root.path())?;
    flow::runtime::validate(&program, &registry).map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let options = flow::runtime::RunOptions {
        command_timeout: Some(Duration::from_secs(30)),
        ..flow::runtime::RunOptions::new(root.to_path_buf())
    };
    let result = flow::runtime::run(&program, &registry, &options)?;
    Ok(serde_json::from_slice(&result.output)?)
}

#[test]
fn caught_parallel_failure_reports_structured_outcome_and_partial_results() -> Result<()> {
    let source = "try\n  parallel at most 2 as group\n    run number.parse_integer with text \"7\" as first\n    run number.parse_integer with text \"bad\" as second\ncatch problem\n  then encode problem as json\n";
    let caught = run_caught(source)?;
    assert_eq!(caught["kind"], "failure", "{caught}");
    assert_eq!(caught["effect_certainty"], "uncertain", "{caught}");
    let group = &caught["task_group"];
    assert_eq!(group["mode"], "join", "{caught}");
    assert_eq!(group["failed_task"], "second", "{caught}");
    assert_eq!(
        group["partial_results_schema_version"],
        serde_json::Value::Null
    );
    let records = group["partial_results"]
        .as_array()
        .expect("partial results are a list");
    assert_eq!(records.len(), 2, "{caught}");
    assert_eq!(records[0]["id"], "first", "{caught}");
    assert_eq!(records[0]["state"], "success", "{caught}");
    assert_eq!(records[1]["id"], "second", "{caught}");
    assert_eq!(records[1]["state"], "non_success", "{caught}");
    Ok(())
}

#[test]
fn caught_race_failure_reports_mode_and_every_task_result() -> Result<()> {
    let source = "try\n  race at most 2 as raced\n    run number.parse_integer with text \"bad\" as first\n    run number.parse_integer with text \"worse\" as second\ncatch problem\n  then encode problem as json\n";
    let caught = run_caught(source)?;
    assert_eq!(caught["kind"], "failure", "{caught}");
    let group = &caught["task_group"];
    assert_eq!(group["mode"], "race", "{caught}");
    assert!(group["failed_task"].is_string(), "{caught}");
    let records = group["partial_results"]
        .as_array()
        .expect("partial results are a list");
    assert_eq!(records.len(), 2, "{caught}");
    Ok(())
}

#[test]
fn caught_parallel_for_each_failure_reports_versioned_partial_results() -> Result<()> {
    // A non-success element is recorded and does not fail the group, so the
    // group failure comes from the shared output limit. The caught value is
    // read directly: encoding it would itself exceed the limit.
    let fixture = serde_json::to_string(env!("CARGO_BIN_EXE_native-fixture"))?;
    let source = format!(
        "parallel for each n in [\"800\", \"800\"] at most 1 as results\n  run {fixture} with arguments [\"repeat-byte\", n]\n"
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = flow::parser::parse(&source)?;
    let manifest_root = tempfile::tempdir()?;
    let registry = bundled_registry(manifest_root.path())?;
    flow::runtime::validate(&program, &registry).map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let options = flow::runtime::RunOptions {
        max_output_bytes: 1500,
        command_timeout: Some(Duration::from_secs(30)),
        ..flow::runtime::RunOptions::new(root.to_path_buf())
    };
    let error = match flow::runtime::run(&program, &registry, &options) {
        Err(error) => error,
        Ok(_) => anyhow::bail!("second element did not exceed the shared output limit"),
    };
    let failure = error
        .downcast_ref::<flow::runtime::tasks::TaskGroupFailure>()
        .expect("task-group failure is typed");
    assert!(
        failure
            .message
            .contains("parallel for each output exceeded 1500 bytes"),
        "unexpected failure cause: {}",
        failure.message
    );
    assert_eq!(failure.partial_results_schema_version, Some(3));

    let value = flow::runtime::execution_result::error_value(&error);
    let flow::ast::Value::Record(fields) = &value else {
        panic!("caught value is a record")
    };
    let Some(flow::ast::Value::Record(group)) = fields.get("task_group") else {
        panic!("caught value has a task_group record: {value:?}")
    };
    assert_eq!(
        group.get("partial_results_schema_version"),
        Some(&flow::ast::Value::Integer(3))
    );
    let Some(flow::ast::Value::List(records)) = group.get("partial_results") else {
        panic!("partial_results is a list")
    };
    assert!(!records.is_empty(), "{value:?}");
    let flow::ast::Value::Record(first) = &records[0] else {
        panic!("first partial result is a record")
    };
    assert_eq!(
        first.get("state"),
        Some(&flow::ast::Value::String("success".into()))
    );
    Ok(())
}
