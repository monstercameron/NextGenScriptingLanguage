//! Issue #250: the item bound is 10,000,000 items, so inputs past the old 100,000
//! cap pass through the list, JSON and construction applets with the same results.

use flow::applets;
use serde_json::{json, Map, Value};

fn run(name: &str, args: Value) -> Result<Value, applets::AppletError> {
    let args: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    applets::execute(name, &args, &[])
}

const ITEMS: usize = 150_000;

#[test]
fn list_range_passes_150000_items() {
    let result = run("list.range", json!({"start": 0, "stop": ITEMS})).unwrap();
    let values = result.as_array().unwrap();
    assert_eq!(values.len(), ITEMS);
    assert_eq!(values[ITEMS - 1], json!(ITEMS - 1));
}

#[test]
fn json_map_over_150000_items_matches_a_smaller_reference() {
    let objects: Vec<Value> = (0..ITEMS).map(|index| json!({"v": index})).collect();
    let result = run("json.map", json!({"document": objects, "select": "/v"})).unwrap();
    let values = result.as_array().unwrap();
    assert_eq!(values.len(), ITEMS);
    assert_eq!(values[0], json!(0));
    assert_eq!(values[ITEMS - 1], json!(ITEMS - 1));

    let reference = run(
        "json.map",
        json!({"document": [{"v": 1}, {"v": 2}], "select": "/v"}),
    )
    .unwrap();
    assert_eq!(reference, json!([1, 2]));
}

#[test]
fn list_make_passes_150000_items() {
    let items: Vec<Value> = (0..ITEMS).map(Value::from).collect();
    let result = run("list.make", json!({"items": items})).unwrap();
    assert_eq!(result.as_array().map(Vec::len), Some(ITEMS));
}

#[test]
fn record_make_passes_150000_fields() {
    let mut fields = Map::new();
    for index in 0..ITEMS {
        fields.insert(format!("k{index:06}"), Value::from(index));
    }
    let result = run("record.make", json!({"fields": fields})).unwrap();
    let record = result.as_object().unwrap();
    assert_eq!(record.len(), ITEMS);
    assert_eq!(record.get("k000000"), Some(&json!(0)));
}
