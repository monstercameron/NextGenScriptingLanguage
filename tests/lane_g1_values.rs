use std::{fs, process::Command};

fn manifests(directory: &std::path::Path) -> std::path::PathBuf {
    let destination = directory.join("manifests");
    fs::create_dir(&destination).unwrap();
    for name in [
        "cat",
        "cd",
        "count.lines",
        "false",
        "grep",
        "head",
        "ls",
        "mkdir",
        "printf",
        "pwd",
        "sort",
        "uniq",
        "wc",
        "core.text_concat",
        "core.text_format",
        "core.json_parse",
        "core.json_stringify",
        "core.json_set",
    ] {
        let file = format!("{name}.json");
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("manifests")
                .join(&file),
            destination.join(file),
        )
        .unwrap();
    }
    destination
}

fn cli(source: &str, command: &str, extra: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("values.program");
    fs::write(&path, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .arg(command)
        .arg(path)
        .arg("--manifest-dir")
        .arg(manifests(dir.path()))
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn repeated_reusable_calls_have_isolated_local_bindings() {
    let source = "define a program named one\n  run printf with message \"local\" and keep status as inner\nrun program one\nrun program one\n";
    let output = cli(source, "check", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mutually_exclusive_branches_can_bind_the_same_name() {
    let source = "accept enabled as boolean with default false\nif enabled is true\n  use value \"yes\" as selected\notherwise\n  use value \"no\" as selected\nuse value selected\n";
    let output = cli(source, "check", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn separate_loops_have_isolated_local_bindings() {
    let source = "accept names as list of text with default [\"one\"]\nfor each first in names\n  run \"printf\" with arguments [first] as local_output\nfor each second in names\n  run \"printf\" with arguments [second] as local_output\n";
    let output = cli(source, "check", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn typed_literals_and_aliases_bind_without_processes() {
    let source = "use value \"CLEAR\" as state\nuse value 3 as count\nuse value true as enabled\nuse value [\"a\", \"b\"] as items\nuse value {\"state\": state, \"count\": count, \"enabled\": enabled, \"items\": items} as record\nuse value record as alias\nthen encode alias as json\n";
    let out = cli(source, "run", &["--trace"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::json!({"state":"CLEAR","count":3,"enabled":true,"items":["a","b"]})
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("\"event\":\"spawn\""));
}

#[test]
fn bound_final_values_are_silent_and_explicit_reuse_prints() {
    for source in [
        "run text.concat with parts [\"internal\"] as name\n",
        "use value \"internal\" as name\n",
        "use value \"internal\"\nthen capture bytes as bytes\n",
    ] {
        let out = cli(source, "run", &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{source}: {:?}", out.stdout);
    }
    let out = cli(
        "use value \"visible\" as name\nuse value name\n",
        "run",
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"visible");
}

#[test]
fn playground_sources_check() {
    let examples: serde_json::Value =
        serde_json::from_str(include_str!("../web/examples.json")).unwrap();
    for example in examples.as_array().unwrap() {
        let out = cli(example["source"].as_str().unwrap(), "check", &[]);
        assert!(
            out.status.success(),
            "{}: {}",
            example["id"],
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn omarchy_programs_parse_and_typecheck_without_output_reset_commands() {
    use flow::ast::{LoopKind, StepKind};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    let mut pending = vec![
        root.join("examples/omarchy-rewrites"),
        root.join("examples/omarchy-ports"),
    ];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "program")
            {
                paths.push(path);
            }
        }
    }
    for path in paths {
        let source = fs::read_to_string(&path).unwrap();
        let program = flow::parser::parse(&source)
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        let operations = program
            .steps
            .iter()
            .chain(
                program
                    .definitions
                    .values()
                    .flat_map(|definition| definition.steps.iter()),
            )
            .filter_map(|step| match &step.kind {
                StepKind::Invoke { call, .. }
                | StepKind::IfCommand { call, .. }
                | StepKind::Loop {
                    kind: LoopKind::WhileCommand { call, .. },
                    ..
                } if !call.direct => Some(call.operation.as_str()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let sources = fs::read_dir(root.join("manifests"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    return None;
                }
                let source = fs::read_to_string(path).unwrap();
                let manifest: serde_json::Value = serde_json::from_str(&source).unwrap();
                let needed = manifest["name"]
                    .as_str()
                    .is_some_and(|name| operations.contains(name))
                    || manifest["phrases"].as_array().is_some_and(|phrases| {
                        phrases.iter().any(|phrase| {
                            phrase
                                .as_str()
                                .is_some_and(|phrase| operations.contains(phrase))
                        })
                    });
                needed.then_some(source)
            })
            .collect::<Vec<_>>();
        let registry = flow::manifest::Registry::from_sources(&sources).unwrap();
        flow::runtime::validate(&program, &registry)
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert!(
            !source
                .lines()
                .any(|line| line.trim() == "run \"printf\" with arguments [\"\"]"),
            "{}",
            path.display()
        );
    }
}

#[test]
fn typed_bindings_are_checked_before_effects_and_explained() {
    for source in [
        "use value 7 as count\nuse value count\n",
        "use value true as flag\nrun text.concat with parts flag\n",
        "use value missing as alias\n",
        "if true is true\n  use value 1 as value\notherwise\n  use value \"text\" as value\n",
    ] {
        let out = cli(source, "check", &[]);
        assert!(!out.status.success(), "{source}");
    }
    let out = cli("use value 7 as count\n", "explain", &["--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan["steps"][0]["kind"], "bind_value");
    assert_eq!(plan["steps"][0]["program_stdout"], "none");
    let out = cli("use value 7 as count\n", "check", &["--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("check.bound_output_withheld"));
}

#[test]
fn branch_bindings_include_byte_captures_and_dynamic_json_selections() {
    for source in [
        "use value \"bytes\"\nif true is true\n  then capture bytes as captured\notherwise\n  then capture bytes as captured\nuse bytes captured\n",
        "accept pointer as text with default \"/value\"\nif true is true\n  take text at pointer from {\"value\": \"yes\"} as selected\notherwise\n  take text at pointer from {\"value\": \"no\"} as selected\nuse value selected\n",
    ] {
        let out = cli(source, "check", &[]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
}

#[cfg(feature = "tooling")]
#[test]
fn shell_compilation_retains_typed_constants_and_silent_bindings() {
    let registry = flow::manifest::Registry::default();
    let source = "use value 7 as count\nuse value {\"count\": count} as record\nthen encode record as json\n";
    let program = flow::parser::parse(source).unwrap();
    let sh = flow::compile_sh::compile(&program, &registry, "values.program", source.as_bytes())
        .unwrap();
    assert!(sh.contains("__execute_emit=0"));
    assert!(sh.contains("{\"count\":7}"));
    let source = "use value 2 as count\nuse value [\"a\", \"b\"] as items\nrepeat count times\n  run \"true\"\nfor each item in items\n  run \"printf\" with arguments [\"%s\", item]\nexit with status count\n";
    let program = flow::parser::parse(source).unwrap();
    assert!(
        flow::compile_sh::compile(&program, &registry, "values.program", source.as_bytes()).is_ok()
    );
    // Runtime-dependent scopes must be refused rather than folded to the wrong value.
    let source = "repeat 0 times\n  use value \"inner\" as name\n";
    let program = flow::parser::parse(source).unwrap();
    assert!(
        flow::compile_sh::compile(&program, &registry, "values.program", source.as_bytes())
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn completed_checkpoint_preserves_bound_output_eligibility() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("resume.program");
    let checkpoint = dir.path().join("checkpoint");
    let manifests = manifests(dir.path());
    fs::write(&source, "use value \"internal\" as name\n").unwrap();
    for command in ["run", "resume"] {
        let out = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(command)
            .arg(&source)
            .arg("--checkpoint")
            .arg(&checkpoint)
            .arg("--manifest-dir")
            .arg(&manifests)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn early_return_keeps_a_bound_program_result_internal() {
    let source = "define a program named helper\n  use value \"internal\"\n  return\nrun program helper as captured\n";
    let out = cli(source, "run", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    let out = cli(&(source.to_owned() + "use value captured\n"), "run", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"internal");
}

#[test]
fn native_status_and_result_captures_withhold_stdout() {
    for suffix in [
        "as name",
        "and keep status as code",
        "and keep result as execution",
    ] {
        let invocation = if cfg!(windows) {
            "run \"cmd\" with arguments [\"/d\", \"/c\", \"echo internal\"]"
        } else {
            "run \"printf\" with arguments [\"internal\"]"
        };
        let source = format!("{invocation} {suffix}\n");
        let out = cli(&source, "run", &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{suffix}: {:?}", out.stdout);
    }
}
