#![cfg(windows)]

use anyhow::{anyhow, Result};
use std::time::Duration;

fn run_source(source: &str, max_output_bytes: usize) -> Result<flow::runtime::RunResult> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = flow::parser::parse(source)?;
    let registry = flow::manifest::Registry::load(&root.join("manifests"))?;
    flow::runtime::validate(&program, &registry)?;
    let options = flow::runtime::RunOptions {
        max_output_bytes,
        command_timeout: Some(Duration::from_secs(30)),
        ..flow::runtime::RunOptions::new(root.to_path_buf())
    };
    flow::runtime::run(&program, &registry, &options)
}

#[test]
fn native_parallel_each_under_limit_keeps_all_encoded_results() -> Result<()> {
    let fixture = serde_json::to_string(env!("CARGO_BIN_EXE_native-fixture"))?;
    let items = ["\"5899\""; 10].join(", ");
    let source = format!(
        "parallel for each n in [{items}] at most 8 as results\n  run {fixture} with arguments [\"repeat-byte\", n]\nthen encode results as json\n"
    );
    let result = run_source(&source, 65_536)?;
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    assert_eq!(value["schema_version"], 3);
    let records = value["items"].as_array().expect("result records");
    assert_eq!(records.len(), 10);
    let raw_bytes: usize = records
        .iter()
        .map(|record| {
            assert_eq!(record["output_encoding"], "utf8");
            record["output"].as_str().expect("utf-8 output").len()
        })
        .sum();
    assert_eq!(raw_bytes, 58_990);
    assert!(raw_bytes < 65_536);
    assert!(result.output.len() > raw_bytes);
    Ok(())
}

#[test]
fn native_parallel_each_accepts_ninety_percent_of_raw_limit() -> Result<()> {
    let fixture = serde_json::to_string(env!("CARGO_BIN_EXE_native-fixture"))?;
    let source = format!(
        "parallel for each n in [\"58982\"] at most 1 as results\n  run {fixture} with arguments [\"repeat-byte\", n]\nthen encode results as json\n"
    );
    let result = run_source(&source, 65_536)?;
    let value: serde_json::Value = serde_json::from_slice(&result.output)?;
    let output = value["items"][0]["output"]
        .as_str()
        .expect("UTF-8 output is stored once");
    assert_eq!(output.len(), 58_982);
    assert_eq!(value["items"][0]["output_encoding"], "utf8");
    Ok(())
}

#[test]
fn native_parallel_each_shared_limit_preserves_versioned_partial_results() -> Result<()> {
    let fixture = serde_json::to_string(env!("CARGO_BIN_EXE_native-fixture"))?;
    // Both invocations fit under 1,500 bytes, while their combined 1,600 do not.
    let source = format!(
        "parallel for each n in [\"800\", \"800\"] at most 1 as results\n  run {fixture} with arguments [\"repeat-byte\", n]\n"
    );
    let error = match run_source(&source, 1500) {
        Err(error) => error,
        Ok(_) => {
            return Err(anyhow!(
                "second native element did not exceed the shared raw output limit"
            ));
        }
    };
    let failure = error
        .downcast_ref::<flow::runtime::tasks::TaskGroupFailure>()
        .expect("task-group failure keeps typed partial results");
    assert!(
        failure
            .message
            .contains("parallel for each output exceeded 1500 bytes"),
        "unexpected failure cause: {}",
        failure.message
    );
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
        first.get("state"),
        Some(&flow::ast::Value::String("success".into()))
    );
    assert_eq!(
        first.get("output"),
        Some(&flow::ast::Value::String("x".repeat(800)))
    );
    Ok(())
}
