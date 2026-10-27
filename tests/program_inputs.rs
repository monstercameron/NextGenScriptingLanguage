#![cfg(unix)]
use flow::{ast::Value, manifest::Registry, parser, runtime};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

fn run(cwd: &Path, source: &str, arguments: &[&str]) -> Output {
    let file = cwd.join("job.program");
    fs::write(&file, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(file)
        .args(arguments)
        .current_dir(cwd)
        .output()
        .unwrap()
}
fn check(result: &Output) {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(cwd.into())
    }
}

#[test]
fn malformed_input_diagnostics_keep_one_line_number_and_actionable_cause() {
    let d = tempfile::tempdir().unwrap();
    for (declaration, cause) in [
        (
            "accept path text",
            "expected `accept <name> as <text|integer|boolean|list of text|list of integer|list of json|json>`",
        ),
        (
            "accept path as string",
            "input type must be text, integer, boolean, list of text, list of integer, list of json, or json",
        ),
        ("accept path as text garbage", "stray token `garbage`"),
        (
            "accept path as text with default",
            "input default: missing value",
        ),
        (
            "accept file-path as text",
            "invalid input name; expected an identifier",
        ),
        (
            "accept files as list of text with default [\"a\"",
            "input default: expected `,` or `]` in list",
        ),
    ] {
        let source = format!("# retain source positions\n{declaration}\n");
        let expected = format!("line 2: {cause}");
        let error = parser::parse(&source).unwrap_err();
        assert_eq!(error.to_string(), expected);
        assert_eq!(format!("{error:#}"), expected);

        let file = d.path().join("invalid.program");
        fs::write(&file, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("check")
            .arg(&file)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let diagnostic = String::from_utf8(result.stderr).unwrap();
        assert!(
            diagnostic.ends_with(&format!("{expected}\n")),
            "{diagnostic}"
        );
        assert_eq!(diagnostic.matches("line 2:").count(), 1, "{diagnostic}");
    }
}

#[test]
fn scalar_inputs_and_literals_format_as_single_native_argv_items() {
    let d = tempfile::tempdir().unwrap();
    let source = "accept count as integer with default 42\naccept enabled as boolean with default true\nrun \"printf\" with arguments [\"%s/%s/%s/%s\", count, enabled, -3, false]\n";
    let result = run(d.path(), source, &[]);
    check(&result);
    assert_eq!(result.stdout, b"42/true/-3/false");
}

#[test]
fn cli_text_defaults_and_override_are_literal_and_preserve_empty_values() {
    let d = tempfile::tempdir().unwrap();
    let source = "accept message as text with default \"default\"\nrun \"printf\" with arguments [\"<%s>\", message]\n";
    let normal = run(d.path(), source, &[]);
    check(&normal);
    assert_eq!(normal.stdout, b"<default>");
    let literal = run(
        d.path(),
        source,
        &["--input", "message=$(touch forbidden); café * = literal"],
    );
    check(&literal);
    assert_eq!(
        literal.stdout,
        "<$(touch forbidden); café * = literal>".as_bytes()
    );
    assert!(!d.path().join("forbidden").exists());
    let empty = run(d.path(), source, &["--input", "message="]);
    check(&empty);
    assert_eq!(empty.stdout, b"<>");
}

#[test]
fn required_file_and_integer_inputs_feed_native_commands_and_parameters() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("a file.txt"), b"one\ntwo\nthree\n").unwrap();
    let source = "accept path as text\naccept count as integer with default 1\nread path\npipe to head with lines count\n";
    let first = run(d.path(), source, &["--input", "path=a file.txt"]);
    check(&first);
    assert_eq!(first.stdout, b"one\n");
    let more = run(
        d.path(),
        source,
        &["--input", "path=a file.txt", "--input", "count=2"],
    );
    check(&more);
    assert_eq!(more.stdout, b"one\ntwo\n");
}

#[test]
fn text_list_inputs_supply_argv_without_joining_or_splitting() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("one file"), [0u8, 255, b'\n']).unwrap();
    fs::write(d.path().join("two"), b"next").unwrap();
    let source = "accept paths as list of text\nrun \"cat\" with arguments paths\n";
    let result = run(
        d.path(),
        source,
        &["--input", r#"paths=["one file","two"]"#],
    );
    check(&result);
    assert_eq!(result.stdout, [0, 255, b'\n', b'n', b'e', b'x', b't']);
    let empty = run(d.path(), source, &["--input", "paths=[]"]);
    check(&empty);
    assert!(empty.stdout.is_empty());
}

#[test]
fn boolean_and_integer_inputs_remain_typed_in_manifest_parameters() {
    let d = tempfile::tempdir().unwrap();
    let manifests = d.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    fs::write(manifests.join("flags.json"),serde_json::json!({"name":"flags","binary":"printf","parameters":{"enabled":{"type":"boolean","required":true},"limit":{"type":"integer","required":true}},"arguments":[{"literal":"%s/%s"},{"parameter":"enabled"},{"parameter":"limit"}]}).to_string()).unwrap();
    let source="accept enabled as boolean with default false\naccept limit as integer with default 10\nrun flags with enabled enabled and limit limit\n";
    let manifest_dir = manifests.to_str().unwrap();
    let result = run(
        d.path(),
        source,
        &[
            "--manifest-dir",
            manifest_dir,
            "--input",
            "enabled=true",
            "--input",
            "limit=-5",
        ],
    );
    check(&result);
    assert_eq!(result.stdout, b"true/-5");
    let result = run(d.path(), source, &["--manifest-dir", manifest_dir]);
    check(&result);
    assert_eq!(result.stdout, b"false/10");
}

#[test]
fn invalid_inputs_are_rejected_before_any_program_effects() {
    let d = tempfile::tempdir().unwrap();
    let source="accept path as text\naccept count as integer with default 1\naccept enabled as boolean with default false\naccept files as list of text with default []\nrun \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\n";
    for arguments in [
        vec![],
        vec!["--input", "unknown=x"],
        vec!["--input", "path=x", "--input", "count=1.5"],
        vec!["--input", "path=x", "--input", "count=9223372036854775808"],
        vec!["--input", "path=x", "--input", "enabled=1"],
        vec!["--input", "path=x", "--input", "files=[1]"],
        vec!["--input", "path=x", "--input", "files=[null]"],
        vec!["--input", "path=x", "--input", "files=[[\"a\"]]"],
        vec!["--input", "path=x", "--input", r#"files=["\u0000"]"#],
        vec!["--input", "path=x", "--input", "path=y"],
        vec!["--input", "broken"],
    ] {
        let result = run(d.path(), source, &arguments);
        assert!(!result.status.success(), "{arguments:?}");
        assert!(!d.path().join("forbidden").exists());
    }
}

#[test]
fn known_input_values_are_checked_against_manifest_constraints_before_effects() {
    let d = tempfile::tempdir().unwrap();
    let source = "accept files as list of text with default []\nrun \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\nrun cat with files files\n";
    let result = run(d.path(), source, &[]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("at least 1"));
    assert!(!d.path().join("forbidden").exists());
}

#[test]
fn integer_list_inputs_iterate_as_integers_and_reject_bad_items_before_effects() {
    let d = tempfile::tempdir().unwrap();
    let source = "accept ports as list of integer\nfor each port in ports\n  run \"printf\" with arguments [\"%s\", port]\n";
    let file = d.path().join("job.program");
    fs::write(&file, source).unwrap();
    for mode in ["check", "run"] {
        let valid = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(mode)
            .arg(&file)
            .args(["--input", "ports=[80,443]"])
            .current_dir(d.path())
            .output()
            .unwrap();
        check(&valid);
        if mode == "run" {
            assert_eq!(valid.stdout, b"80443");
        }
    }

    let source = "accept ports as list of integer\nrun \"touch\" with arguments [\"forbidden\"]\n";
    fs::write(&file, source).unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&file)
        .args(["--input", "ports=[80,\"bad\"]"])
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(!d.path().join("forbidden").exists());
}

#[test]
fn declaration_order_defaults_and_types_are_checked_before_execution() {
    for source in [
        "accept name as text\naccept name as text",
        "accept true as text",
        "accept path as unknown",
        "accept count as integer with default \"1\"",
        "accept files as list of text with default [false]",
        "accept path as text with default other",
        "accept path as text garbage",
        "run pwd\naccept path as text",
        "every 5 milliseconds\n  accept path as text",
    ] {
        assert!(parser::parse(source).is_err(), "{source}");
    }
    let registry =
        Registry::load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    for source in [
        "accept count as integer\nread count",
        "accept count as text\nrun printf with message \"effect\"\nthen head with lines count",
        "accept args as list of text\nrun \"printf\" with arguments [args]",
        "accept path as text\nrun pwd as path",
        "accept event as text\nlisten to stdin\nwhenever a line arrives as event\n  then cat",
    ] {
        let program = parser::parse(source).unwrap();
        assert!(runtime::validate(&program, &registry).is_err(), "{source}");
    }
}

#[test]
fn check_and_explain_describe_required_inputs_without_requesting_values() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("job.program");
    fs::write(
        &p,
        "accept path as text\nrun \"cat\" with arguments [path]\n",
    )
    .unwrap();
    for mode in ["check", "explain"] {
        let out = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(mode)
            .arg(&p)
            .current_dir(d.path())
            .output()
            .unwrap();
        check(&out);
        if mode == "explain" {
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(
                text.contains("accept path as text; required input"),
                "{text}"
            );
        }
    }
}

#[test]
fn input_defaults_and_overrides_work_in_listener_sources_and_handlers() {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir(d.path().join("nested")).unwrap();
    let source="accept folder as text with default \"nested\"\naccept content as text\naccept count as integer with default 1\nrun cd with path folder\nlisten to \"printf\" with arguments [\"%s\", content]\nwhenever a line arrives as event\n  then head with lines count\n";
    let result = run(
        d.path(),
        source,
        &["--input", "content=one\ntwo\n", "--max-runs", "2"],
    );
    check(&result);
    assert_eq!(result.stdout, b"one\ntwo\n");
}

#[test]
fn interval_inputs_survive_fresh_handler_contexts() {
    let d = tempfile::tempdir().unwrap();
    let source="accept message as text with default \"tick\"\nevery 1 milliseconds\n  run \"printf\" with arguments [\"%s\\n\", message]\n";
    let result = run(
        d.path(),
        source,
        &["--input", "message=actual", "--max-runs", "2"],
    );
    check(&result);
    assert_eq!(result.stdout, b"actual\nactual\n");
}

#[test]
fn stream_headers_accept_declared_values_with_original_line_numbers() {
    let d = tempfile::tempdir().unwrap();
    let source="# leading comment\naccept\tmessage as text\n\nstream from \"printf\" with arguments [\"%s\", message]\npipe to \"tr\" with arguments [\"[:lower:]\", \"[:upper:]\"]\nshow each line\n";
    let parsed = flow::stream::parse(source).unwrap();
    assert_eq!(parsed.pipeline.inputs[0].line, 2);
    assert_eq!(parsed.pipeline.steps[0].line, 4);
    let result = run(d.path(), source, &["--input", "message=one\ntwo"]);
    check(&result);
    assert_eq!(result.stdout, b"ONE\nTWO");
}

#[test]
fn completed_checkpoint_requires_the_same_effective_input_values() {
    let d = tempfile::tempdir().unwrap();
    let source="accept message as text with default \"first\"\nrun \"printf\" with arguments [\"%s\", message]\nthen save to \"result\"\n";
    let cp = d.path().join("checkpoint");
    let first = run(d.path(), source, &["--checkpoint", cp.to_str().unwrap()]);
    check(&first);
    let file = d.path().join("job.program");
    let same = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("resume")
        .arg(&file)
        .args([
            "--checkpoint",
            cp.to_str().unwrap(),
            "--input",
            "message=first",
            "--trace",
        ])
        .current_dir(d.path())
        .output()
        .unwrap();
    check(&same);
    assert!(same.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&same.stderr).contains("\"event\":\"spawn\""));
    let changed = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("resume")
        .arg(&file)
        .args([
            "--checkpoint",
            cp.to_str().unwrap(),
            "--input",
            "message=changed",
        ])
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(!changed.status.success());
    assert_eq!(fs::read(d.path().join("result")).unwrap(), b"first");
}

#[test]
fn library_context_supplies_declared_values_and_rejects_wrong_runtime_types() {
    let d = tempfile::tempdir().unwrap();
    let program =
        parser::parse("accept message as text\nrun \"printf\" with arguments [\"%s\", message]\n")
            .unwrap();
    let registry = Registry::default();
    let result = runtime::run_with_context(
        &program,
        &registry,
        &options(d.path()),
        BTreeMap::from([("message".into(), Value::String("from host".into()))]),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(result.output, b"from host");
    let mut opts = options(d.path());
    opts.inputs.insert("message".into(), Value::Integer(4));
    assert!(runtime::run(&program, &registry, &opts).is_err());
    let plain = parser::parse("run \"true\"\n").unwrap();
    assert!(runtime::run_with_context(
        &plain,
        &registry,
        &opts,
        BTreeMap::new(),
        Vec::new(),
        Arc::new(AtomicBool::new(false))
    )
    .is_err());
    let triggered = parser::parse("every 1 seconds\n  run \"true\"\n").unwrap();
    assert!(runtime::run_with_context(
        &triggered,
        &registry,
        &options(d.path()),
        BTreeMap::new(),
        Vec::new(),
        Arc::new(AtomicBool::new(false))
    )
    .is_err());
}

#[test]
fn migrated_utilities_use_cli_paths_instead_of_the_absent_original_files() {
    let script = r#"
import copy,importlib.util,json,os
from pathlib import Path
root=Path(os.environ['EXECUTE_PROJECT'])
spec=importlib.util.spec_from_file_location('utility_runner',root/'examples/ladder/run.py')
runner=importlib.util.module_from_spec(spec);spec.loader.exec_module(runner)
binary=Path(os.environ['EXECUTE_INPUT_BINARY'])
for original in runner.load_catalog():
 if original['number'] not in (1,3,6,76):continue
 case=copy.deepcopy(original);replacements={}
 for index,(name,parameter) in enumerate(case['parameters'].items()):
  target=f'alternate/{index} input with spaces'
  replacements[parameter['default']]=target
  case.setdefault('args',[]).extend(['--input',f'{name}={target}'])
 for field in ('fixtures','fixtures_base64'):
  if field in case:case[field]={replacements.get(name,name):value for name,value in case[field].items()}
 result=runner.run_case(binary,case)
 print(json.dumps({'case':case['number'],'overrides':replacements,'native_result':result}))
"#;
    let result = Command::new("python3")
        .args(["-c", script])
        .env("EXECUTE_PROJECT", env!("CARGO_MANIFEST_DIR"))
        .env("EXECUTE_INPUT_BINARY", env!("CARGO_BIN_EXE_execute"))
        .output()
        .unwrap();
    check(&result);
    assert_eq!(
        result
            .stdout
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        4
    );
}
