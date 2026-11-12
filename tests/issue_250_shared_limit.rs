//! Issue #250: the shared list input bound is the one 10,000,000-item bound, so a list
//! past the old 1,000,000 cap passes through the list applets with the same result.

use flow::applets;
use serde_json::{json, Map, Value};

fn run(name: &str, args: Value) -> Result<Value, applets::AppletError> {
    let args: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    applets::execute(name, &args, &[])
}

const ITEMS: usize = 1_000_010;

#[test]
fn list_make_passes_past_the_old_million_item_bound() {
    let items: Vec<Value> = (0..ITEMS).map(|index| json!(index % 10)).collect();
    let result = run("list.make", json!({"items": items})).unwrap();
    let values = result.as_array().unwrap();
    assert_eq!(values.len(), ITEMS);
    assert_eq!(values[ITEMS - 1], json!((ITEMS - 1) % 10));

    let reference = run("list.make", json!({"items": [7, 8, 9]})).unwrap();
    assert_eq!(reference, json!([7, 8, 9]));
}
