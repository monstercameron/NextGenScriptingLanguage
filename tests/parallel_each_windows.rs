#![cfg(windows)]

use anyhow::{anyhow, Result};
use std::time::Duration;

#[test]
fn native_parallel_each_limit_keeps_versioned_partial_results() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = serde_json::to_string(env!("CARGO_BIN_EXE_native-fixture"))?;
    let source = format!(
        "parallel for each n in [\"2\", \"2000\"] at most 1 as results\n  run {fixture} with arguments [\"repeat-byte\", n]\n"
    );
    let program = flow::parser::parse(&source)?;
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
                "second native element did not exceed the shared raw output limit"
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
