#![cfg(unix)]
use flow::{ast::Value, manifest::Registry, parser, runtime};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
    time::Duration,
};

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.into(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: None,
    }
}
fn run(cwd: &Path, source: &str) -> runtime::RunResult {
    runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &options(cwd),
    )
    .unwrap()
}
fn cli(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}
fn good(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_output_fields_keep_types_for_arguments_and_conditions() {
    let dir = tempfile::tempdir().unwrap();
    let result = run(dir.path(), "run \"printf\" with arguments [\"%s\", \"{\\\"name\\\":\\\"雪; $HOME\\\",\\\"stars\\\":42,\\\"active\\\":true}\"]\nthen parse json as repo\ntake text at \"/name\" from repo as name\ntake integer at \"/stars\" from repo as stars\ntake boolean at \"/active\" from repo as active\nif active is true\n  then \"printf\" with arguments [\"%s %s\", name, stars]\notherwise\n  run \"false\"\n");
    assert_eq!(result.output, "雪; $HOME 42".as_bytes());
    assert_eq!(result.launched_processes, 2);
}

#[test]
fn json_pointers_cover_arrays_escapes_root_empty_keys_and_null() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("input.json"),
        r#"{"a/b":{"~":[{"":null},"雪"]},"names":["one","two"]}"#,
    )
    .unwrap();
    for (pointer, kind, wanted) in [
        ("/a~1b/~0/0/", "json", "null"),
        ("/a~1b/~0/1", "text", "\"雪\""),
        ("/names", "list of text", "[\"one\",\"two\"]"),
    ] {
        let result = run(dir.path(), &format!("read \"input.json\"\nthen parse json as doc\ntake {kind} at {pointer:?} from doc as selected\nthen encode selected as json\n"));
        assert_eq!(result.output, wanted.as_bytes());
        assert_eq!(result.launched_processes, 0);
    }
    let result = run(dir.path(), "read \"input.json\"\nthen parse json as doc\ntake json at \"\" from doc as whole\nthen encode whole as json\n");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.output).unwrap(),
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(dir.path().join("input.json")).unwrap()
        )
        .unwrap()
    );
}

#[test]
fn missing_keys_null_wrong_types_and_invalid_array_indexes_fail_before_later_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("input.json"),
        r#"{"nil":null,"n":1.5,"flag":true,"items":[7],"names":["ok",4]}"#,
    )
    .unwrap();
    for (pointer, kind) in [
        ("/missing", "text"),
        ("/nil", "text"),
        ("/n", "integer"),
        ("/flag", "integer"),
        ("/items/01", "integer"),
        ("/items/-", "integer"),
        ("/items/+0", "integer"),
        ("/items/99999999999999999999999999", "integer"),
        ("/names", "list of text"),
    ] {
        let source=format!("read \"input.json\"\nthen parse json as doc\ntake {kind} at {pointer:?} from doc as bad\nthen save to \"forbidden\"\n");
        let error = runtime::run(
            &parser::parse(&source).unwrap(),
            &Registry::default(),
            &options(dir.path()),
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("line 3"));
        assert!(!dir.path().join("forbidden").exists());
    }
}

#[test]
fn malformed_syntax_names_types_and_branch_bindings_are_rejected_before_effects() {
    for invalid in [
        "then parse json as 2bad",
        "then parse yaml as doc",
        "take text at \"name\" from doc as value",
        "take text at \"/~2\" from doc as value",
        "take text at 2 from doc as value",
        "take number at \"/n\" from doc as n",
        "then encode doc as yaml",
    ] {
        assert!(parser::parse(&format!("{invalid}\n")).is_err(), "{invalid}");
    }
    let dir = tempfile::tempdir().unwrap();
    for invalid in ["then parse json as doc\n", "take text at \"/name\" from missing as value\n", "accept d as json\nrun \"printf\" with arguments [d]\n", "accept pointer as integer\naccept doc as json\ntake text at pointer from doc as value\n", "run \"printf\" with arguments [\"{}\"]\nthen parse json as doc\ntake text at \"/name\" from doc as doc\n", "run \"printf\" with arguments [\"{}\"]\nif true is true\n  then parse json as doc\nthen encode doc as json\n"] {
        let program = parser::parse(invalid).unwrap();
        assert!(runtime::validate(&program, &Registry::default()).is_err(), "{invalid}");
    }
    let program=parser::parse("run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\ntake integer at \"\" from true as wrong\n").unwrap();
    assert!(runtime::run(&program, &Registry::default(), &options(dir.path())).is_err());
    assert!(!dir.path().join("forbidden").exists());

    let unbound_pointer = parser::parse(
        "accept doc as json\nrun \"printf\" with arguments [\"effect\"]\nthen save to \"pointer-forbidden\"\ntake text at pointer from doc as value\n",
    )
    .unwrap();
    assert!(runtime::run(&unbound_pointer, &Registry::default(), &options(dir.path())).is_err());
    assert!(!dir.path().join("pointer-forbidden").exists());
}

#[test]
fn invalid_known_json_input_selection_fails_before_earlier_native_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.program"), "accept data as json\nrun \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\ntake integer at \"/count\" from data as count\n").unwrap();
    let output = cli(
        dir.path(),
        &["main.program", "--input", r#"data={"count":"wrong"}"#],
    );
    assert!(!output.status.success());
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn json_stdin_and_nested_reference_encoding_keep_literal_boundaries() {
    use std::{io::Write, process::Stdio};
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.program"), "read from stdin\nthen parse json as doc\ntake text at \"/name\" from doc as name\nthen encode [name, true, 7] as json\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .arg("main.program")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"name":"$(touch forbidden); *"}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    good(&output);
    assert_eq!(output.stdout, br#"["$(touch forbidden); *",true,7]"#);
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn parse_preserves_exact_input_bytes_and_encoding_is_explicit_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let original = b" { \"text\" : \"line\\nvalue\" } \n";
    fs::write(dir.path().join("input.json"), original).unwrap();
    let source = "read \"input.json\"\nthen parse json as doc\n";
    assert_eq!(run(dir.path(), source).output, original);
    let source="read \"input.json\"\nthen parse json as doc\nthen encode doc as json\npipe to \"wc\" with arguments [\"-c\"]\n";
    let result = run(dir.path(), source);
    assert_eq!(String::from_utf8(result.output).unwrap().trim(), "22");
    assert_eq!(result.launched_processes, 1);
    let mut bounded = options(dir.path());
    bounded.max_output_bytes = 8;
    let source = "accept text as text\nthen encode text as json\nthen save to \"forbidden\"\n";
    bounded
        .inputs
        .insert("text".into(), Value::String("\n\n\n\n".into()));
    let error = runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &bounded,
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("maximum output bytes"));
    assert!(!dir.path().join("forbidden").exists());
    assert!(runtime::run(
        &parser::parse("read \"input.json\"\nthen parse json as doc\n").unwrap(),
        &Registry::default(),
        &bounded
    )
    .is_err());
}

#[test]
fn malformed_trailing_invalid_utf8_and_excessive_depth_json_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for bytes in [
        b"{} {}".to_vec(),
        b"{bad}".to_vec(),
        b"\"\xff\"".to_vec(),
        [vec![b'['; 140], vec![b']'; 140]].concat(),
    ] {
        fs::write(dir.path().join("input.json"), bytes).unwrap();
        assert!(runtime::run(
            &parser::parse("read \"input.json\"\nthen parse json as doc\n").unwrap(),
            &Registry::default(),
            &options(dir.path())
        )
        .is_err());
    }
}

#[test]
fn json_numbers_and_nul_survive_encoding_but_are_not_implicitly_native_scalars() {
    let dir = tempfile::tempdir().unwrap();
    let numbers = r#"[18446744073709551615,1.25,1e100,null,"\u0000"]"#;
    fs::write(dir.path().join("input.json"), numbers).unwrap();
    let result = run(
        dir.path(),
        "read \"input.json\"\nthen parse json as doc\nthen encode doc as json\n",
    );
    let encoded: serde_json::Value = serde_json::from_slice(&result.output).unwrap();
    assert_eq!(encoded[0].as_u64(), Some(u64::MAX));
    assert_eq!(encoded[1].as_f64(), Some(1.25));
    assert_eq!(encoded[2].as_f64(), Some(1e100));
    assert_eq!(encoded[3], serde_json::Value::Null);
    assert_eq!(encoded[4].as_str(), Some("\0"));
    for (pointer, kind) in [
        ("/0", "integer"),
        ("/1", "integer"),
        ("/2", "integer"),
        ("/4", "text"),
    ] {
        assert!(runtime::run(&parser::parse(&format!("read \"input.json\"\nthen parse json as doc\ntake {kind} at {pointer:?} from doc as value\n")).unwrap(), &Registry::default(), &options(dir.path())).is_err());
    }
}

#[test]
fn ordinary_object_keys_are_never_internal_number_tags() {
    let dir = tempfile::tempdir().unwrap();
    let text = r#"{"$serde_json::private::Number":"7","other":true}"#;
    fs::write(dir.path().join("input.json"), text).unwrap();
    let result = run(dir.path(), "read \"input.json\"\nthen parse json as doc\ntake text at \"/$serde_json::private::Number\" from doc as field\nthen encode doc as json\n");
    assert_eq!(result.output, text.as_bytes());
    fs::write(dir.path().join("input.json"), "1e400").unwrap();
    assert!(runtime::run(
        &parser::parse("read \"input.json\"\nthen parse json as doc\n").unwrap(),
        &Registry::default(),
        &options(dir.path())
    )
    .is_err());
}

#[test]
fn cli_json_inputs_inspection_and_input_checkpoint_identity_are_real() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.program"), "accept data as json\ntake integer at \"/count\" from data as count\nthen \"printf\" with arguments [\"%s\", count]\n").unwrap();
    let output = cli(
        dir.path(),
        &[
            "main.program",
            "--input",
            r#"data={"count":7}"#,
            "--checkpoint",
            "journal",
        ],
    );
    good(&output);
    assert_eq!(output.stdout, b"7");
    let output = cli(
        dir.path(),
        &[
            "resume",
            "main.program",
            "--input",
            r#"data={"count":7}"#,
            "--checkpoint",
            "journal",
        ],
    );
    good(&output);
    assert_eq!(output.stdout, b"7");
    assert!(!cli(
        dir.path(),
        &[
            "resume",
            "main.program",
            "--input",
            r#"data={"count":8}"#,
            "--checkpoint",
            "journal"
        ]
    )
    .status
    .success());
    let output = cli(
        dir.path(),
        &["inspect", "main.program", "--input", r#"data={"count":7}"#],
    );
    good(&output);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["inputs"][0]["type"], "json");
    assert_eq!(result["inputs"][0]["value"]["count"], 7);
    assert!(!cli(dir.path(), &["main.program", "--input", "data={bad}"])
        .status
        .success());
}

#[test]
fn checkpoint_restores_structured_values_and_all_branch_result_types() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("input.json"),
        r#"{"nested":[null,1.5,18446744073709551615],"name":"restored"}"#,
    )
    .unwrap();
    let prefix = "read \"input.json\"\nthen parse json as doc\n";
    fs::write(
        dir.path().join("main.program"),
        format!("{prefix}run \"false\"\n"),
    )
    .unwrap();
    assert!(
        !cli(dir.path(), &["main.program", "--checkpoint", "journal"])
            .status
            .success()
    );
    fs::write(dir.path().join("input.json"), "{}").unwrap();
    fs::write(dir.path().join("main.program"),format!("{prefix}if true is true\n  take json at \"/nested\" from doc as selected\notherwise\n  take json at \"/name\" from doc as selected\nthen encode selected as json\n")).unwrap();
    let output = cli(
        dir.path(),
        &[
            "resume",
            "main.program",
            "--checkpoint",
            "journal",
            "--retry-uncertain",
        ],
    );
    good(&output);
    assert_eq!(output.stdout, b"[null,1.5,18446744073709551615]");
    let output = cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"],
    );
    good(&output);
    assert_eq!(output.stdout, b"[null,1.5,18446744073709551615]");
}

#[test]
fn scheduled_and_listener_handlers_reuse_the_same_structured_instructions() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("events.jsonl"),
        "{\"name\":\"one\"}\n{\"name\":\"two\"}\n",
    )
    .unwrap();
    let source="listen to \"cat\" with arguments [\"events.jsonl\"]\nwhenever a line arrives as event\n  then parse json as doc\n  take text at \"/name\" from doc as name\n  then \"printf\" with arguments [\"%s\\n\", name]\n";
    fs::write(dir.path().join("listen.program"), source).unwrap();
    let output = cli(dir.path(), &["listen.program", "--max-runs", "2"]);
    good(&output);
    assert_eq!(output.stdout, b"one\ntwo\n");
    fs::write(dir.path().join("scheduled.program"),"accept data as json\nevery 1 milliseconds\n  take text at \"/name\" from data as name\n  then \"printf\" with arguments [\"%s\\n\", name]\n").unwrap();
    let output = cli(
        dir.path(),
        &[
            "scheduled.program",
            "--input",
            r#"data={"name":"tick"}"#,
            "--max-runs",
            "2",
        ],
    );
    good(&output);
    assert_eq!(output.stdout, b"tick\ntick\n");
}
