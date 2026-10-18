use anyhow::Result;
use flow::ast::{Step, StepKind, Value};
use flow::manifest::Registry;
use flow::runtime::{run, RunOptions};
use std::collections::BTreeMap;

fn execute(source: &str, inputs: BTreeMap<String, Value>) -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    let program = flow::parser::parse(source)?;
    let registry = Registry::from_sources(&[] as &[&str])?;
    let result = run(
        &program,
        &registry,
        &RunOptions {
            inputs,
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(std::time::Duration::from_secs(5)),
            total_timeout: Some(std::time::Duration::from_secs(10)),
            ..RunOptions::new(directory.path().to_path_buf())
        },
    )?;
    Ok(result.output)
}

#[test]
fn break_continue_and_return_transfer_at_their_lexical_targets() -> Result<()> {
    let broken = execute(
        "for each item in [\"1\", \"2\", \"3\"]\n  if item is \"2\"\n    break\n  run \"printf\" with arguments [item]\n",
        BTreeMap::new(),
    )?;
    assert_eq!(broken, b"1");

    let continued = execute(
        "run \"printf\" with arguments [\"before\"]\nfor each item in [1]\n  if item is 1\n    continue\n  run \"printf\" with arguments [\"wrong\"]\n",
        BTreeMap::new(),
    )?;
    assert_eq!(continued, b"before");

    let returned = execute(
        "define a program named early\n  run \"printf\" with arguments [\"kept\"]\n  return\n  run \"printf\" with arguments [\"wrong\"]\nrun program early\n",
        BTreeMap::new(),
    )?;
    assert_eq!(returned, b"kept");
    Ok(())
}

#[test]
fn reusable_call_end_offsets_restore_nested_frames_after_early_return() -> Result<()> {
    let output = execute(
        "define a program named leaf\n  run \"printf\" with arguments [\"nested\"]\ndefine a program named early\n  try\n    run program leaf as local\n    return\n  finally\n    run \"printf\" with arguments [\"finished\"]\n  run \"printf\" with arguments [\"wrong\"]\naccept local as text with default \"root\"\nrun program leaf as simple\nrun program early as result\nthen \"printf\" with arguments [\"%s|%s|%s\", simple, result, local]\n",
        BTreeMap::new(),
    )?;
    // Child bindings remain local; nested call output is restored into `local`
    // only inside `early`, and return still visits EndCall to bind its finalizer
    // output to `result`.
    assert_eq!(output, b"nested|finished|root");
    Ok(())
}

#[test]
fn json_iteration_dynamic_pointer_and_structured_conditions_execute() -> Result<()> {
    let iterated = execute(
        "accept items as json\nfor each item in items\n  then encode item as json\n",
        BTreeMap::from([(
            "items".into(),
            Value::List(vec![
                Value::Integer(1),
                Value::Record(BTreeMap::from([(
                    "name".into(),
                    Value::String("Ada".into()),
                )])),
            ]),
        )]),
    )?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&iterated)?,
        serde_json::json!({"name":"Ada"})
    );

    let selected = execute(
        "accept pointer as text\ntake text at pointer from {\"user\": {\"name\": \"Ada\"}} as name\nthen encode name as json\n",
        BTreeMap::from([("pointer".into(), Value::String("/user/name".into()))]),
    )?;
    assert_eq!(selected, br#""Ada""#);

    let present = execute(
        "accept document as json\nif document is null\n  then encode {\"found\": false} as json\notherwise\n  if document contains \"name\"\n    then encode {\"found\": true} as json\n  otherwise\n    then encode {\"found\": false} as json\n",
        BTreeMap::from([(
            "document".into(),
            Value::Record(BTreeMap::from([(
                "name".into(),
                Value::String("Ada".into()),
            )])),
        )]),
    )?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&present)?,
        serde_json::json!({"found":true})
    );
    Ok(())
}

#[test]
fn finally_runs_across_break_and_catch_receives_a_structured_error() -> Result<()> {
    let finalized = execute(
        "repeat 2 times\n  try\n    break\n  finally\n    run \"printf\" with arguments [\"cleaned\"]\n",
        BTreeMap::new(),
    )?;
    assert_eq!(finalized, b"cleaned");

    let normal_try = execute(
        "run \"printf\" with arguments [\"ok\"]\nthen save to \"try-marker\"\ntry\n  run \"true\" with arguments []\ncatch failure\n  then encode failure as json\n  then save to \"try-marker\"\nread file \"try-marker\"\n",
        BTreeMap::new(),
    )?;
    assert_eq!(normal_try, b"ok");

    let caught = execute(
        "try\n  run \"false\" with arguments []\ncatch failure\n  then encode failure as json\n",
        BTreeMap::new(),
    )?;
    let record: serde_json::Value = serde_json::from_slice(&caught)?;
    assert_eq!(record["kind"], "failure");
    assert!(record.get("effect_certainty").is_some());
    assert_eq!(record["stdout_captured"], false);
    Ok(())
}

#[test]
fn parser_rejects_transfers_outside_their_lexical_scope() {
    for source in ["break", "continue", "return"] {
        assert!(flow::parser::parse(source).is_err(), "accepted {source:?}");
    }
    assert!(flow::parser::parse("repeat 1 times\n  return\n").is_err());
    assert!(flow::parser::parse("define a program named p\n  break\n").is_err());
}

#[test]
fn validator_rejects_forged_control_flow_ast_without_panicking() -> Result<()> {
    let registry = Registry::from_sources(&[] as &[&str])?;

    let mut orphan_transfer = flow::parser::parse("run \"pwd\"\n")?;
    orphan_transfer.steps.push(Step {
        line: 2,
        label: None,
        kind: StepKind::BreakLoop,
    });
    assert!(flow::runtime::validate(&orphan_transfer, &registry).is_err());

    let mut orphan_return = flow::parser::parse("run \"pwd\"\n")?;
    orphan_return.steps.push(Step {
        line: 2,
        label: None,
        kind: StepKind::Return,
    });
    assert!(flow::runtime::validate(&orphan_return, &registry).is_err());

    let mut malformed_try = flow::parser::parse("run \"pwd\"\n")?;
    malformed_try.steps[0].kind = StepKind::Try {
        catch: None,
        finally: None,
        end: usize::MAX,
    };
    assert!(flow::runtime::validate(&malformed_try, &registry).is_err());
    Ok(())
}

#[test]
fn legacy_condition_json_omits_the_default_operator() -> Result<()> {
    let program = flow::parser::parse("if true is true\n  run pwd\n")?;
    let json = serde_json::to_value(&program.steps[0].kind)?;
    assert!(json.get("operator").is_none());
    Ok(())
}
