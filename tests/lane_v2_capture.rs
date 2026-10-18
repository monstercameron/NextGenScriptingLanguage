use flow::{manifest::Registry, parser, runtime};
use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    let name = if cfg!(windows) {
        "native-fixture.exe"
    } else {
        "native-fixture"
    };
    let executable = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(name);
    assert!(
        executable.is_file(),
        "build --example native-fixture before running these tests"
    );
    executable
}

fn source(arguments: &[&str]) -> String {
    format!(
        "run {} with arguments {}\n",
        serde_json::to_string(&fixture()).unwrap(),
        serde_json::to_string(arguments).unwrap()
    )
}

fn options(cwd: &Path, limit: usize) -> runtime::RunOptions {
    let mut options = runtime::RunOptions::new(cwd.to_path_buf());
    options.max_output_bytes = limit;
    options.command_timeout = Some(std::time::Duration::from_secs(10));
    options
}

#[test]
fn lane_v2_native_capture_empty_short_and_full_limit() {
    let root = tempfile::tempdir().unwrap();
    for text in ["", "a", "two words\n"] {
        let program = parser::parse(&source(&["write", text])).unwrap();
        let result = runtime::run(
            &program,
            &Registry::default(),
            &options(root.path(), text.len()),
        )
        .unwrap();
        assert_eq!(result.output, text.as_bytes());
        if text.is_empty() {
            assert_eq!(
                result.output.capacity(),
                0,
                "empty captures need no allocation"
            );
        }
    }
}

#[test]
fn lane_v2_native_capture_output_limit_preserves_exact_prefix() {
    let root = tempfile::tempdir().unwrap();
    let program = parser::parse(&source(&["flood"])).unwrap();
    for limit in [0, 1, 32767, 32768, 32769, 4 * 1024 * 1024 - 1] {
        let error = runtime::run(&program, &Registry::default(), &options(root.path(), limit))
            .err()
            .expect("limit must fail closed");
        let failure = error
            .downcast_ref::<runtime::execution_result::ExecutionFailure>()
            .expect("typed native output-limit failure");
        assert_eq!(
            failure.result.kind,
            runtime::execution_result::ExecutionKind::OutputLimit
        );
        let output = failure
            .result
            .stdout
            .as_ref()
            .expect("bounded failure output");
        assert_eq!(output.len(), limit);
        assert!(output.iter().all(|byte| *byte == b'x'));
    }
}

#[test]
fn lane_v2_native_pipeline_keeps_descriptor_wiring_and_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut program = source(&["flood"]);
    program.push_str(&format!(
        "pipe to {} with arguments [\"copy\"]\n",
        serde_json::to_string(&fixture()).unwrap()
    ));
    let result = runtime::run(
        &parser::parse(&program).unwrap(),
        &Registry::default(),
        &options(root.path(), 4 * 1024 * 1024),
    )
    .unwrap();
    assert_eq!(result.output.len(), 4 * 1024 * 1024);
    assert!(result.output.iter().all(|byte| *byte == b'x'));
}

#[test]
fn lane_v2_preflighted_background_argv_keeps_empty_and_spaced_arguments() {
    let root = tempfile::tempdir().unwrap();
    let command = source(&["argv", "", "two words", "last"]);
    let source = format!("spawn as stage\n  {}await stage\n", command);
    let result = runtime::run(
        &parser::parse(&source).unwrap(),
        &Registry::default(),
        &options(root.path(), 1024),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&result.output).unwrap(),
        ["", "two words", "last"]
    );
}
