//! Issue #364: the published schemas for `execute.inspect/1` and `execute.gate/1`
//! accept the output the library builds for real programs and reject damage.
//! The output comes from `flow::inputs::describe` and
//! `flow::agent::gate::verdict` over plans from `flow::agent::explain::explain_json`,
//! with manifests supplied inline (no `manifests/` folder is loaded from disk).
//! The checker implements the keywords these schemas use: type, const, enum,
//! required, properties, additionalProperties (false or a schema), items, minimum,
//! oneOf. It is the same checker as `tests/agent_schemas.rs`.

use flow::ast::Value as ProgramValue;
use flow::manifest::Registry;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn project() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn load_schema(relative: &str) -> Value {
    let text = std::fs::read_to_string(project().join(relative))
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
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| {
                let mut branch_errors = Vec::new();
                violations(branch, instance, path, &mut branch_errors);
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
                    Some(sub) => violations(sub, value, &format!("{path}/{key}"), errors),
                    None if closed => errors.push(format!("{path}: unexpected property {key}")),
                    None => {
                        if let Some(sub) =
                            schema.get("additionalProperties").filter(|s| s.is_object())
                        {
                            violations(sub, value, &format!("{path}/{key}"), errors);
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    violations(item_schema, item, &format!("{path}/{index}"), errors);
                }
            }
        }
        _ => {}
    }
}

fn assert_conforms(schema_file: &str, instance: &Value) {
    let schema = load_schema(schema_file);
    let mut errors = Vec::new();
    violations(&schema, instance, "", &mut errors);
    assert!(
        errors.is_empty(),
        "{schema_file} rejected real output: {errors:#?}\n{instance:#}"
    );
}

fn assert_rejects(schema_file: &str, damaged: &Value) {
    let schema = load_schema(schema_file);
    let mut errors = Vec::new();
    violations(&schema, damaged, "", &mut errors);
    assert!(
        !errors.is_empty(),
        "{schema_file} accepted a damaged document: {damaged:#}"
    );
}

const INSPECT: &str = "docs/schemas/inspect.v1.schema.json";
const GATE: &str = "docs/schemas/gate.v1.schema.json";

#[test]
fn inspect_schema_accepts_describe_output_for_every_input_source() {
    let source = "\
accept name as text
accept mode as text one of [\"light\", \"dark\"] described as \"Theme\" with default \"light\"
accept count as integer from argument 1 with default 2
accept verbose as boolean from flag \"--verbose\" alias \"-v\"
accept output as text from option \"--output\" alias \"-o\" with default \"out.txt\"
accept home_dir as text from public environment \"HOME\"
accept token as text from environment \"FLOW_TOKEN\"
accept key as text from secret prompt \"Key?\"
accept here as text from program directory
accept where as text from program path
accept extra as list of text from arguments
";
    let program = flow::parser::parse(source).unwrap();
    let supplied: BTreeMap<String, ProgramValue> = BTreeMap::from([
        ("name".to_owned(), ProgramValue::String("alice".to_owned())),
        ("mode".to_owned(), ProgramValue::String("dark".to_owned())),
        ("key".to_owned(), ProgramValue::String("s3cret".to_owned())),
        ("token".to_owned(), ProgramValue::String("tok".to_owned())),
    ]);
    let object = flow::inputs::describe(&program.inputs, &supplied).unwrap();
    assert_conforms(INSPECT, &object);

    assert_eq!(object["protocol"], "execute.inspect/1");
    assert_eq!(object["inputs_complete"], false);
    let missing: Vec<&str> = object["missing_inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    assert!(missing.contains(&"where") && missing.contains(&"extra"));
    let key = object["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|input| input["name"] == "key")
        .unwrap();
    assert_eq!(key["value_withheld"], true);
    assert!(key.get("value").is_none(), "a secret value must not print");

    let mut unknown_source = object.clone();
    unknown_source["inputs"][0]["source"] = json!({"kind": "stdin"});
    assert_rejects(INSPECT, &unknown_source);
    let mut bad_type = object.clone();
    bad_type["inputs"][0]["type"] = json!("float");
    assert_rejects(INSPECT, &bad_type);
    let mut complete = object.clone();
    complete["inputs_complete"] = json!("yes");
    assert_rejects(INSPECT, &complete);
    let mut wrong_protocol = object.clone();
    wrong_protocol["protocol"] = json!("execute.explain/2");
    assert_rejects(INSPECT, &wrong_protocol);
    let mut extra = object;
    extra["surprise"] = json!(true);
    assert_rejects(INSPECT, &extra);
}

#[test]
fn gate_schema_accepts_real_verdicts_and_rejects_damage() {
    let registry = Registry::from_sources(&[
        json!({"name": "net.helper", "binary": "net.helper", "arguments": [],
               "contract": {"version": 1, "capabilities": ["network.loopback"], "effects": ["network.loopback"]}})
        .to_string(),
        json!({"name": "writer.helper", "binary": "writer.helper", "arguments": [],
               "contract": {"version": 1, "capabilities": ["fs.write"], "effects": ["fs.write"]}})
        .to_string(),
        json!({"name": "plain.helper", "binary": "plain.helper", "arguments": []}).to_string(),
    ])
    .unwrap();
    let program = flow::parser::parse(
        "accept token as text from secret prompt \"Token?\"\n\
         run net.helper\n\
         run plain.helper\n\
         run writer.helper and retry up to 1 time\n",
    )
    .unwrap();
    let plan = flow::agent::explain::explain_json(&program, &registry, "gate.program").unwrap();
    let set = |names: &[&str]| -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    };

    let refused = flow::agent::gate::verdict(&plan, &set(&["network"]), None);
    assert_conforms(GATE, &refused);
    assert_eq!(refused["allowed"], false);
    assert_eq!(refused["prompts"].as_array().unwrap().len(), 1);
    assert_eq!(refused["violations"].as_array().unwrap().len(), 2);
    assert!(refused["violations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|violation| violation["reason"] == "not_approved"));

    let undeclared = flow::agent::gate::verdict(
        &plan,
        &set(&["network", "writes", "undeclared"]),
        Some(&set(&["network", "writes"])),
    );
    assert_conforms(GATE, &undeclared);
    assert!(undeclared["violations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|violation| violation["reason"] == "not_declared"));

    let approved =
        flow::agent::gate::verdict(&plan, &set(&["network", "writes", "undeclared"]), None);
    assert_conforms(GATE, &approved);
    assert_eq!(approved["allowed"], true);
    assert_eq!(approved["violations"], json!([]));

    let mut bad_verdict = refused.clone();
    bad_verdict["allowed"] = json!("yes");
    assert_rejects(GATE, &bad_verdict);
    let mut bad_reason = refused.clone();
    bad_reason["violations"][0]["reason"] = json!("maybe");
    assert_rejects(GATE, &bad_reason);
    let mut bad_category = refused.clone();
    bad_category["violations"][0]["category"] = json!("teleport");
    assert_rejects(GATE, &bad_category);
    let mut extra = refused;
    extra["surprise"] = json!(1);
    assert_rejects(GATE, &extra);
}
