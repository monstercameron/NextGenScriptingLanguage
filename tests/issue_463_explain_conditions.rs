//! Issue #463, first open criterion: the `explain` plan names the condition
//! that guards an `if` row. The plan comes from the parsed program through
//! `flow::agent::explain::explain_json_with_structure`; nothing runs. The
//! published shape (`explain_json_with_inputs`) must stay unchanged.

use flow::agent::explain;
use flow::manifest::Registry;
use flow::parser;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const SAMPLE: &str = "accept items as list of text with default [\"a\"]
accept mode as text with default \"x\"
for each item in items
  if mode is \"x\"
    run \"rm\" with arguments [\"-f\", item]
  otherwise
    run \"touch\" with arguments [item]
";

fn rows_of_kind<'a>(plan: &'a Value, kind: &str) -> Vec<&'a Value> {
    plan["steps"]
        .as_array()
        .expect("steps array")
        .iter()
        .filter(|row| row["kind"] == kind)
        .collect()
}

#[test]
fn if_row_names_its_guard_with_reference_and_literal_operands() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "guard.program",
        &BTreeMap::new(),
    )
    .expect("structure plan");

    let ifs = rows_of_kind(&plan, "if");
    assert_eq!(ifs.len(), 1, "one if row for the sample program");
    assert_eq!(
        ifs[0]["condition"],
        json!({
            "left": {"ref": "mode"},
            "op": "equals",
            "negated": false,
            "right": {"literal": "x"}
        })
    );
}

#[test]
fn loop_and_otherwise_rows_keep_the_existing_fields() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "guard.program",
        &BTreeMap::new(),
    )
    .expect("structure plan");

    // A loop's source is in `iterates`, never in `condition`. Only `if` and `while` rows have one.
    assert_eq!(rows_of_kind(&plan, "loop_for_each").len(), 1);
    assert!(rows_of_kind(&plan, "loop_for_each")[0]
        .get("condition")
        .is_none());
    assert!(rows_of_kind(&plan, "otherwise")[0]
        .get("condition")
        .is_none());
}

#[test]
fn condition_shows_the_reference_name_not_the_supplied_input_value() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let mut inputs = BTreeMap::new();
    inputs.insert(
        "mode".to_string(),
        flow::ast::Value::String("supplied".to_string()),
    );
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "guard.program",
        &inputs,
    )
    .expect("structure plan");

    let ifs = rows_of_kind(&plan, "if");
    assert_eq!(ifs[0]["condition"]["left"], json!({"ref": "mode"}));
}

#[test]
fn negated_condition_is_marked_negated() {
    let source = "accept mode as text with default \"x\"
if mode is not \"x\"
  run \"touch\" with arguments [\"a\"]
";
    let program = parser::parse(source).expect("negated sample parses");
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "negated.program",
        &BTreeMap::new(),
    )
    .expect("structure plan");

    let ifs = rows_of_kind(&plan, "if");
    assert_eq!(ifs[0]["condition"]["negated"], json!(true));
    assert_eq!(ifs[0]["condition"]["op"], json!("equals"));
}

#[test]
fn published_shape_has_no_condition_key() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let plan = explain::explain_json_with_inputs(
        &program,
        &Registry::default(),
        "guard.program",
        &BTreeMap::new(),
    )
    .expect("published plan");

    for row in plan["steps"].as_array().expect("steps array") {
        assert!(
            row.get("condition").is_none(),
            "published rows must not gain a condition key: {row}"
        );
    }
    assert_eq!(rows_of_kind(&plan, "if").len(), 1);
}

// Issue #463, further structure: loop sources, bindings, operand references,
// nesting and the while condition. Each property is optional and appears only on
// the rows it applies to.

/// A declared input, a loop over a bound list, an `if` inside the loop and a step
/// after the loop.
const LOOP_SOURCE: &str = "accept mode as text with default \"x\"
use value [\"a\", \"b\"] as names
for each item in names
  run \"printf\" with arguments [\"%s\", item] as picked
  if mode is \"x\"
    run \"rm\" with arguments [\"-f\", picked]
  otherwise
    run \"touch\" with arguments [picked]
run \"echo\" with arguments [\"done\"]
";

const WHILE_SOURCE: &str = "accept mode as text with default \"x\"
while mode is \"x\"
  run \"printf\" with arguments [\"tick\"] as tick
run \"echo\" with arguments [\"done\"]
";

const PARALLEL_SOURCE: &str = "accept urls as list of text with default [\"a\"]
parallel for each url in urls at most 2 as results
  run \"curl\" with arguments [\"-sf\", url]
run \"echo\" with arguments [\"done\"]
";

fn plan_for(source: &str, inputs: &BTreeMap<String, flow::ast::Value>) -> Value {
    let program = parser::parse(source).expect("program parses");
    let registry = Registry::default();
    let mut plan =
        explain::explain_json_with_structure(&program, &registry, "structure.program", inputs)
            .expect("structure plan");
    // `explain --json` adds this field too, so the schema sees the same document.
    flow::capabilities::annotate_enforcement(&mut plan, &registry);
    plan
}

fn plan_rows(plan: &Value) -> Vec<Value> {
    plan["steps"].as_array().expect("steps array").clone()
}

fn kinds(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row["kind"].as_str().expect("row kind"))
        .collect()
}

#[test]
fn loop_row_names_its_source_and_loop_variable_only() {
    let plan = plan_for(LOOP_SOURCE, &BTreeMap::new());
    let rows = plan_rows(&plan);
    assert_eq!(
        kinds(&rows),
        [
            "bind_value",
            "loop_for_each",
            "invoke",
            "if",
            "invoke",
            "otherwise",
            "invoke",
            "end_if",
            "end_loop",
            "invoke"
        ]
    );
    assert_eq!(
        rows[1]["iterates"],
        json!({"variable": "item", "source": {"ref": "names"}})
    );
    for (index, row) in rows.iter().enumerate() {
        if index != 1 {
            assert!(
                row.get("iterates").is_none(),
                "row {index} ({}) is not a loop",
                row["kind"]
            );
        }
    }
}

#[test]
fn bindings_and_operand_references_sit_on_their_steps() {
    let plan = plan_for(LOOP_SOURCE, &BTreeMap::new());
    let rows = plan_rows(&plan);

    // `use value` names its binding; a literal list references no binding.
    assert_eq!(rows[0]["binds"], json!("names"));
    assert!(rows[0].get("reads_bindings").is_none());
    // The loop reads the list it iterates over. Its variable is in `iterates`.
    assert_eq!(rows[1]["reads_bindings"], json!(["names"]));
    assert!(rows[1].get("binds").is_none());
    // The command binds its stdout and reads the loop variable.
    assert_eq!(rows[2]["binds"], json!("picked"));
    assert_eq!(rows[2]["reads_bindings"], json!(["item"]));
    // The `if` reads the input it tests, by name.
    assert_eq!(rows[3]["reads_bindings"], json!(["mode"]));
    assert!(rows[3].get("binds").is_none());
    // A command without `as` binds nothing; it reads the binding it is given.
    assert!(rows[4].get("binds").is_none());
    assert_eq!(rows[4]["reads_bindings"], json!(["picked"]));
    assert_eq!(rows[6]["reads_bindings"], json!(["picked"]));
    // A step with neither a binding nor a reference has neither key.
    assert!(rows[5].get("binds").is_none());
    assert!(rows[5].get("reads_bindings").is_none());
    assert!(rows[9].get("binds").is_none());
    assert!(rows[9].get("reads_bindings").is_none());
}

#[test]
fn repeated_operand_references_are_listed_once_in_first_use_order() {
    let source = "accept name as text with default \"n\"
run \"printf\" with arguments [\"%s %s\", name, name] as out
";
    let plan = plan_for(source, &BTreeMap::new());
    let rows = plan_rows(&plan);
    assert_eq!(rows[0]["reads_bindings"], json!(["name"]));
    assert_eq!(rows[0]["binds"], json!("out"));
}

#[test]
fn depth_and_parent_follow_the_block_structure() {
    let plan = plan_for(LOOP_SOURCE, &BTreeMap::new());
    let rows = plan_rows(&plan);
    let depths: Vec<u64> = rows
        .iter()
        .map(|row| row["depth"].as_u64().expect("every row has a depth"))
        .collect();
    assert_eq!(depths, [0, 0, 1, 1, 2, 1, 2, 1, 0, 0]);

    let loop_id = rows[1]["id"].clone();
    let if_id = rows[3]["id"].clone();
    assert!(rows[0].get("parent").is_none());
    assert_eq!(rows[2]["parent"], loop_id);
    assert_eq!(rows[3]["parent"], loop_id);
    assert_eq!(rows[4]["parent"], if_id);
    assert_eq!(
        rows[5]["parent"], loop_id,
        "otherwise sits at the if's level"
    );
    assert_eq!(rows[6]["parent"], if_id);
    assert_eq!(
        rows[7]["parent"], loop_id,
        "end_if closes the if in the loop"
    );
    assert!(rows[8].get("parent").is_none(), "end_loop is top level");
    assert!(rows[9].get("parent").is_none());

    // Every nested row sits exactly one level below the row it names as parent.
    for row in &rows {
        if let Some(parent) = row.get("parent") {
            let parent_row = rows
                .iter()
                .find(|candidate| &candidate["id"] == parent)
                .expect("parent is a row of this plan");
            assert_eq!(
                row["depth"].as_u64(),
                parent_row["depth"].as_u64().map(|d| d + 1)
            );
        }
    }
}

#[test]
fn while_row_carries_its_condition_and_nests_its_body() {
    let plan = plan_for(WHILE_SOURCE, &BTreeMap::new());
    let rows = plan_rows(&plan);
    assert_eq!(kinds(&rows), ["loop_while", "invoke", "end_loop", "invoke"]);
    assert_eq!(
        rows[0]["condition"],
        json!({
            "left": {"ref": "mode"},
            "op": "equals",
            "negated": false,
            "right": {"literal": "x"}
        })
    );
    assert_eq!(rows[0]["reads_bindings"], json!(["mode"]));
    assert!(rows[0].get("iterates").is_none());
    assert_eq!(rows[1]["depth"], json!(1));
    assert_eq!(rows[1]["parent"], rows[0]["id"]);
    assert_eq!(rows[1]["binds"], json!("tick"));
}

#[test]
fn parallel_for_each_task_row_names_its_source_and_group_binding() {
    let plan = plan_for(PARALLEL_SOURCE, &BTreeMap::new());
    let rows = plan_rows(&plan);
    assert_eq!(kinds(&rows), ["task_group", "invoke"]);
    assert_eq!(
        rows[0]["iterates"],
        json!({"variable": "url", "source": {"ref": "urls"}})
    );
    assert_eq!(rows[0]["binds"], json!("results"));
    // The source list is read first, then the task's own arguments.
    assert_eq!(rows[0]["reads_bindings"], json!(["urls", "url"]));
    assert_eq!(rows[0]["depth"], json!(0));
    assert!(rows[1].get("iterates").is_none());
}

#[test]
fn supplied_input_value_never_appears_in_the_structure_plan() {
    let mut inputs = BTreeMap::new();
    inputs.insert(
        "mode".to_string(),
        flow::ast::Value::String("supplied-mode-value".to_string()),
    );
    let plan = plan_for(LOOP_SOURCE, &inputs);
    let text = plan.to_string();
    assert!(
        !text.contains("supplied-mode-value"),
        "a supplied input value leaked into the plan: {text}"
    );
    assert_eq!(
        plan_rows(&plan)[3]["condition"]["left"],
        json!({"ref": "mode"})
    );
}

#[test]
fn published_shape_has_none_of_the_structure_keys() {
    let program = parser::parse(LOOP_SOURCE).expect("program parses");
    let plan = explain::explain_json_with_inputs(
        &program,
        &Registry::default(),
        "structure.program",
        &BTreeMap::new(),
    )
    .expect("published plan");
    for row in plan_rows(&plan) {
        for key in [
            "depth",
            "parent",
            "iterates",
            "binds",
            "reads_bindings",
            "condition",
        ] {
            assert!(
                row.get(key).is_none(),
                "published row has the structure key {key}: {row}"
            );
        }
    }
}

// The checker of tests/agent_schemas.rs, which validates explain.v2 output. It
// resolves local `$ref`s; this copy implements the keywords the explain schemas use.
fn load_schema(relative: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading {relative}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parsing {relative}: {error}"))
}

fn matches_type(instance: &Value, kind: &str) -> bool {
    match kind {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "integer" => instance.is_i64() || instance.is_u64(),
        "number" => instance.is_number(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => false,
    }
}

fn violations(schema: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    violations_inner(schema, schema, instance, path, errors);
}

fn violations_inner(
    root: &Value,
    schema: &Value,
    instance: &Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = root
            .pointer(reference.strip_prefix('#').expect("local schema reference"))
            .expect("valid schema reference");
        violations_inner(root, target, instance, path, errors);
    }
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            violations_inner(root, branch, instance, path, errors);
        }
    }
    if let Some(condition) = schema.get("if") {
        let mut conditional_errors = Vec::new();
        violations_inner(root, condition, instance, path, &mut conditional_errors);
        if conditional_errors.is_empty() {
            if let Some(then) = schema.get("then") {
                violations_inner(root, then, instance, path, errors);
            }
        } else if let Some(otherwise) = schema.get("else") {
            violations_inner(root, otherwise, instance, path, errors);
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        if !branches.iter().any(|branch| {
            let mut branch_errors = Vec::new();
            violations_inner(root, branch, instance, path, &mut branch_errors);
            branch_errors.is_empty()
        }) {
            errors.push(format!("{path}: no anyOf branch matches"));
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| {
                let mut branch_errors = Vec::new();
                violations_inner(root, branch, instance, path, &mut branch_errors);
                branch_errors.is_empty()
            })
            .count();
        if matching != 1 {
            errors.push(format!(
                "{path}: matches {matching} oneOf branches, expected exactly 1"
            ));
        }
    }
    if let Some(constant) = schema.get("const") {
        if constant != instance {
            errors.push(format!("{path}: expected {constant}"));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(instance) {
            errors.push(format!("{path}: {instance} is not an allowed value"));
        }
    }
    if let Some(kind) = schema.get("type") {
        let kinds: Vec<&str> = match kind {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !kinds.iter().any(|name| matches_type(instance, name)) {
            errors.push(format!("{path}: expected type {kind}"));
            return;
        }
    }
    if let (Some(minimum), Some(number)) = (
        schema.get("minimum").and_then(Value::as_f64),
        instance.as_f64(),
    ) {
        if number < minimum {
            errors.push(format!("{path}: {number} is below {minimum}"));
        }
    }
    match instance {
        Value::Object(map) => {
            for name in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = name.as_str() {
                    if !map.contains_key(name) {
                        errors.push(format!("{path}: missing required property {name}"));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
            for (key, value) in map {
                match properties.and_then(|known| known.get(key)) {
                    Some(sub) => {
                        violations_inner(root, sub, value, &format!("{path}/{key}"), errors)
                    }
                    None if closed => errors.push(format!("{path}: unexpected property {key}")),
                    None => {
                        if let Some(sub) =
                            schema.get("additionalProperties").filter(|s| s.is_object())
                        {
                            violations_inner(root, sub, value, &format!("{path}/{key}"), errors);
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    violations_inner(root, item_schema, item, &format!("{path}/{index}"), errors);
                }
            }
        }
        _ => {}
    }
}

fn assert_conforms_v2(plan: &Value) {
    let schema = load_schema("docs/schemas/explain.v2.schema.json");
    let mut errors = Vec::new();
    violations(&schema, plan, "", &mut errors);
    assert!(
        errors.is_empty(),
        "explain.v2 rejected the structure plan: {errors:#?}"
    );
}

fn assert_rejects_v2(damaged: &Value) {
    let schema = load_schema("docs/schemas/explain.v2.schema.json");
    let mut errors = Vec::new();
    violations(&schema, damaged, "", &mut errors);
    assert!(
        !errors.is_empty(),
        "explain.v2 accepted a damaged plan: {damaged:#}"
    );
}

#[test]
fn structure_plans_validate_against_the_explain_v2_schema() {
    for source in [LOOP_SOURCE, WHILE_SOURCE, PARALLEL_SOURCE] {
        assert_conforms_v2(&plan_for(source, &BTreeMap::new()));
    }

    let plan = plan_for(LOOP_SOURCE, &BTreeMap::new());
    let mut extra_source_key = plan.clone();
    extra_source_key["steps"][1]["iterates"]["source"]["value"] = json!("a");
    assert_rejects_v2(&extra_source_key);

    let mut text_depth = plan.clone();
    text_depth["steps"][2]["depth"] = json!("deep");
    assert_rejects_v2(&text_depth);

    let mut null_binding = plan.clone();
    null_binding["steps"][2]["binds"] = Value::Null;
    assert_rejects_v2(&null_binding);
}
