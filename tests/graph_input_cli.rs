//! Graphs use the same typed process-input contract as finite workflows.
#![cfg(feature = "graph")]
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    program: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("graph.program");
        fs::write(&program, source).unwrap();
        Self { directory, program }
    }

    fn command(&self, action: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(self.directory.path())
            .arg(action)
            .arg(&self.program)
            .arg("--manifest-dir")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"));
        command
    }
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn port_value(report: &Value, node: &str) -> flow::ast::Value {
    let event = report["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["port"]["node"] == node && event["port"]["port"] == "value")
        .unwrap();
    assert_eq!(event["payload"]["kind"], "value");
    ast_value_from_json(&event["payload"]["value"])
}

fn ast_value_from_json(value: &Value) -> flow::ast::Value {
    match value {
        Value::Null => flow::ast::Value::Null,
        Value::Bool(value) => flow::ast::Value::Bool(*value),
        Value::Number(value) => value
            .as_i64()
            .map(flow::ast::Value::Integer)
            .unwrap_or_else(|| flow::ast::Value::Number(value.clone())),
        Value::String(value) => flow::ast::Value::String(value.clone()),
        Value::Array(values) => {
            flow::ast::Value::List(values.iter().map(ast_value_from_json).collect())
        }
        Value::Object(values) => flow::ast::Value::Record(
            values
                .iter()
                .map(|(key, value)| (key.clone(), ast_value_from_json(value)))
                .collect(),
        ),
    }
}

#[test]
fn named_and_positional_inputs_reach_typed_graph_nodes() {
    let fixture = Fixture::new(
        "accept name as text from argument 1 with default \"world\"\n\
         accept count as integer with default 2\n\
         graph greeting\n\
           run text.concat with parts [\"hello\", name] and separator \" \" as message\n\
           run number.add with left count and right 1 as incremented\n\
         end graph\n",
    );
    let report = success(
        fixture
            .command("run")
            .args(["--input", "count=4", "--", "Snow 雪 with spaces"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        port_value(&report, "message"),
        flow::ast::Value::String("hello Snow 雪 with spaces".into())
    );
    assert_eq!(
        port_value(&report, "incremented"),
        flow::ast::Value::Integer(5)
    );

    let overridden = success(
        fixture
            .command("run")
            .args(["--input", "name=named", "--", "positional"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        port_value(&overridden, "message"),
        flow::ast::Value::String("hello named".into())
    );
    assert_eq!(
        port_value(&overridden, "incremented"),
        flow::ast::Value::Integer(3)
    );
}

#[test]
fn inspection_describes_missing_environment_inputs_without_reading_them() {
    let fixture = Fixture::new(
        "accept message as text from environment \"EXECUTE_GRAPH_INPUT_TEST\"\n\
         graph inspectable\n\
           run text.concat with parts [message] as message_node\n\
         end graph\n",
    );
    let report = success(
        fixture
            .command("inspect")
            .env("EXECUTE_GRAPH_INPUT_TEST", "runtime value")
            .output()
            .unwrap(),
    );
    assert_eq!(report["protocol"], "execute.graph/1");
    assert_eq!(
        report["interface"]["missing_inputs"],
        serde_json::json!(["message"])
    );
    assert_eq!(report["interface"]["inputs_complete"], false);
    assert_eq!(
        report["interface"]["inputs"][0]["source"]["kind"],
        "environment"
    );
    assert!(report["interface"]["inputs"][0].get("value").is_none());

    let run = success(
        fixture
            .command("run")
            .env("EXECUTE_GRAPH_INPUT_TEST", "runtime value")
            .output()
            .unwrap(),
    );
    assert_eq!(
        port_value(&run, "message_node"),
        flow::ast::Value::String("runtime value".into())
    );
}

#[test]
fn inspection_withholds_supplied_sensitive_inputs_from_nested_parameters() {
    let prompt = "GRAPH_PROMPT_CANARY_793";
    let environment = "GRAPH_ENV_CANARY_793";
    for (declaration, canary) in [
        ("accept token as text from secret prompt \"Token?\"", prompt),
        (
            "accept token as text from environment \"EXECUTE_GRAPH_PRIVATE_INPUT\"",
            environment,
        ),
    ] {
        let fixture = Fixture::new(&format!(
            "{declaration}\naccept label as text\ngraph inspectable\n  run text.concat with parts [\"prefix\", token, label] and separator \"|\" as composed\nend graph\n"
        ));
        let report = success(
            fixture
                .command("inspect")
                .args([
                    "--input",
                    &format!("token={canary}"),
                    "--input",
                    "label=visible-control",
                ])
                .output()
                .unwrap(),
        );
        let output = serde_json::to_string(&report).unwrap();
        assert!(!output.contains(canary), "secret leaked in {output}");
        assert_eq!(report["interface"]["inputs"][0]["value_withheld"], true);
        assert!(
            output.contains("visible-control"),
            "non-secret value was lost: {output}"
        );
        assert!(
            output.contains("token"),
            "sensitive reference was lost: {output}"
        );
    }
}

#[test]
fn invalid_sensitive_graph_input_is_redacted_for_every_static_and_runtime_command() {
    let canary = "GRAPH_ENUM_CANARY_793";
    let fixture = Fixture::new(
        "accept mode as text from secret prompt \"Mode?\"\ngraph invalid\n  run text.trim with text \"safe\" and mode mode as trimmed\nend graph\n",
    );
    for action in ["check", "inspect", "explain", "run"] {
        let output = fixture
            .command(action)
            .args(["--input", &format!("mode={canary}")])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{action} accepted invalid enum input"
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !combined.contains(canary),
            "{action} leaked sensitive input: {combined}"
        );
        assert!(combined.contains("must be one of"), "{action}: {combined}");
    }
}

#[test]
fn runtime_environment_precedes_defaults_and_named_values_precede_environment() {
    let fixture = Fixture::new(
        "accept message as text from environment \"EXECUTE_GRAPH_INPUT_TEST\" with default \"default\"\n\
         graph configured\n\
           run text.concat with parts [message] as message_node\n\
         end graph\n",
    );
    let report = success(
        fixture
            .command("run")
            .env("EXECUTE_GRAPH_INPUT_TEST", "environment")
            .output()
            .unwrap(),
    );
    assert_eq!(
        port_value(&report, "message_node"),
        flow::ast::Value::String("environment".into())
    );
    let named = success(
        fixture
            .command("run")
            .env("EXECUTE_GRAPH_INPUT_TEST", "environment")
            .args(["--input", "message=named"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        port_value(&named, "message_node"),
        flow::ast::Value::String("named".into())
    );
}

#[test]
fn check_and_explain_allow_missing_required_values_and_validate_overrides() {
    let fixture = Fixture::new(
        "accept parts as list of text\n\
         graph partial\n\
           run text.concat with parts parts as joined\n\
         end graph\n",
    );
    for action in ["check", "explain"] {
        let output = fixture.command(action).output().unwrap();
        assert!(
            output.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let invalid = fixture
            .command(action)
            .args(["--input", "parts=42"])
            .output()
            .unwrap();
        assert!(
            !invalid.status.success(),
            "{action} accepted a scalar for a list input"
        );
    }
    let report = success(fixture.command("check").arg("--json").output().unwrap());
    assert_eq!(report["ok"], true);
}

#[test]
fn all_required_inputs_are_resolved_before_any_graph_effect() {
    let fixture = Fixture::new(
        "accept message as text\n\
         graph preflight\n\
           run fs.write_atomic with path \"marker.txt\" and expected_content_hash \"missing\" and symlink_policy \"reject\" and permissions_policy \"private\" and content \"started\" as writer\n\
           run text.concat with parts [message] as message_node\n\
         end graph\n",
    );
    let output = fixture.command("run").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing required input"));
    assert!(!fixture.directory.path().join("marker.txt").exists());
}

#[test]
fn legacy_graphs_reject_undeclared_cli_values() {
    let fixture = Fixture::new(
        "graph legacy\n  run text.concat with parts [\"hello\"] as message\nend graph\n",
    );
    for arguments in [vec!["--input", "unknown=value"], vec!["--", "unexpected"]] {
        let output = fixture.command("run").args(arguments).output().unwrap();
        assert!(!output.status.success());
    }
}
