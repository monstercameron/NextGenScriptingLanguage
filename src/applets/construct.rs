//! In-process construction applets for records and lists (issue #234).
//!
//! `record.make` and `list.make` build a typed JSON value from bindings, `record.set`
//! sets one field and `list.append` extends a list. Each takes the applet argument map
//! and returns a JSON value. Values pass through unchanged, so a string containing
//! quotes, newlines or JSON-looking text stays a string; nothing is built by
//! concatenation or escaped by hand.
//!
//! Standalone: this file depends only on `serde_json` and `std`, so an integration
//! test can compile it with `#[path]`. The limits below are copies of
//! `applets::shared::{MAX_BYTES, MAX_DEPTH}` (16 MiB, 128 levels) and
//! `applets::list::ITEM_LIMIT` (10,000,000 items). They are generous on purpose.
//! Wiring replaces them with the shared constants and maps `ConstructError` to
//! `invalid(message)`.

use serde_json::{Map, Value};
use std::fmt;
use std::io::{self, Write};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_ITEMS: usize = 10_000_000;

/// Error returned by the construction applets. Every error is a bad request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstructError {
    pub message: String,
}

impl fmt::Display for ConstructError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn invalid(message: impl Into<String>) -> ConstructError {
    ConstructError {
        message: message.into(),
    }
}

/// `record.make`: `args.fields` is a record; returns it with keys in sorted order.
pub fn record_make(args: &Map<String, Value>) -> Result<Value, ConstructError> {
    let fields = object_arg(args, "fields")?;
    let mut entries: Vec<(&str, &Value)> = fields
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    check_count(entries.len())?;
    entries.sort_by(|left, right| left.0.cmp(right.0));
    finish(Value::Object(entries_to_map(entries)))
}

/// `record.set`: `args.record`, `args.key` (string) and `args.value` (any JSON); returns a
/// new record with `key` set to `value`, keys in sorted order. The input is not changed.
pub fn record_set(args: &Map<String, Value>) -> Result<Value, ConstructError> {
    let record = object_arg(args, "record")?;
    let key = string_arg(args, "key")?;
    let value = args
        .get("value")
        .ok_or_else(|| invalid("args.value is required"))?;
    let mut entries: Vec<(&str, &Value)> = record
        .iter()
        .map(|(existing, current)| (existing.as_str(), current))
        .filter(|(existing, _)| *existing != key)
        .collect();
    entries.push((key, value));
    check_count(entries.len())?;
    entries.sort_by(|left, right| left.0.cmp(right.0));
    finish(Value::Object(entries_to_map(entries)))
}

/// `list.make`: `args.items` is a list; returns it. `args.flatten` is not implemented
/// and is refused rather than ignored.
pub fn list_make(args: &Map<String, Value>) -> Result<Value, ConstructError> {
    if args.contains_key("flatten") {
        return Err(invalid("args.flatten is not implemented"));
    }
    let items = array_arg(args, "items")?;
    check_count(items.len())?;
    finish(Value::Array(items.clone()))
}

/// `list.append`: `args.list` is a list; `args.item` appends one value (a list value is
/// appended as one element), `args.items` appends each element of a list. Exactly one
/// of the two must be given. The input list is not changed.
pub fn list_append(args: &Map<String, Value>) -> Result<Value, ConstructError> {
    let list = array_arg(args, "list")?;
    let added: Vec<&Value> = match (args.get("item"), args.get("items")) {
        (Some(_), Some(_)) => {
            return Err(invalid("args.item and args.items cannot both be set"));
        }
        (Some(item), None) => vec![item],
        (None, Some(Value::Array(items))) => items.iter().collect(),
        (None, Some(_)) => return Err(invalid("args.items must be an array")),
        (None, None) => return Err(invalid("args.item is required")),
    };
    check_count(list.len().saturating_add(added.len()))?;
    let mut extended = Vec::with_capacity(list.len() + added.len());
    extended.extend(list.iter().cloned());
    extended.extend(added.into_iter().cloned());
    finish(Value::Array(extended))
}

fn object_arg<'a>(
    args: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Map<String, Value>, ConstructError> {
    match args.get(name) {
        Some(Value::Object(fields)) => Ok(fields),
        Some(_) => Err(invalid(format!("args.{name} must be an object"))),
        None => Err(invalid(format!("args.{name} is required"))),
    }
}

fn array_arg<'a>(
    args: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Vec<Value>, ConstructError> {
    match args.get(name) {
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(invalid(format!("args.{name} must be an array"))),
        None => Err(invalid(format!("args.{name} is required"))),
    }
}

fn string_arg<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, ConstructError> {
    match args.get(name) {
        Some(Value::String(text)) => Ok(text.as_str()),
        Some(_) => Err(invalid(format!("args.{name} must be a string"))),
        None => Err(invalid(format!("args.{name} is required"))),
    }
}

fn entries_to_map(entries: Vec<(&str, &Value)>) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_owned(), value.clone());
    }
    map
}

fn check_count(count: usize) -> Result<(), ConstructError> {
    if count > MAX_ITEMS {
        return Err(invalid(format!(
            "value exceeds the bounded {MAX_ITEMS}-item limit"
        )));
    }
    Ok(())
}

fn finish(value: Value) -> Result<Value, ConstructError> {
    check_shape(&value)?;
    check_encoded_size(&value)?;
    Ok(value)
}

/// Walks the whole value without recursion: refuses any container over the item
/// limit and any node deeper than the depth limit (the root is level 1).
fn check_shape(root: &Value) -> Result<(), ConstructError> {
    let mut pending: Vec<(&Value, usize)> = vec![(root, 1)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            return Err(invalid(format!(
                "JSON value exceeds the {MAX_DEPTH}-level depth limit"
            )));
        }
        match value {
            Value::Array(items) => {
                check_count(items.len())?;
                pending.extend(items.iter().map(|item| (item, depth + 1)));
            }
            Value::Object(fields) => {
                check_count(fields.len())?;
                pending.extend(fields.values().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Counts the encoded bytes and stops writing as soon as the limit is passed.
struct LimitedCounter {
    written: usize,
}

impl Write for LimitedCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.written = self.written.saturating_add(buffer.len());
        if self.written > MAX_BYTES {
            return Err(io::Error::other("encoded size limit"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_encoded_size(root: &Value) -> Result<(), ConstructError> {
    let mut counter = LimitedCounter { written: 0 };
    match serde_json::to_writer(&mut counter, root) {
        Ok(()) => Ok(()),
        Err(_) if counter.written > MAX_BYTES => Err(invalid(format!(
            "JSON value exceeds the {MAX_BYTES}-byte limit"
        ))),
        Err(error) => Err(invalid(format!("JSON value cannot be serialized: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> Map<String, Value> {
        value
            .as_object()
            .cloned()
            .expect("test arguments are an object")
    }

    fn text(value: &Value) -> String {
        serde_json::to_string(value).expect("value serializes")
    }

    /// `[[...[null]...]]` with `levels` brackets.
    fn nested(levels: usize) -> Value {
        (0..levels).fold(Value::Null, |inner, _| Value::Array(vec![inner]))
    }

    #[test]
    fn record_make_sorts_keys_and_keeps_values() {
        let result = record_make(&args(serde_json::json!({
            "fields": {"b": 1, "a": "x", "c": [true, null]}
        })))
        .expect("record builds");
        assert_eq!(text(&result), r#"{"a":"x","b":1,"c":[true,null]}"#);
    }

    #[test]
    fn record_make_is_deterministic_for_any_insertion_order() {
        let mut forward = Map::new();
        let mut backward = Map::new();
        for key in ["alpha", "beta", "gamma", "delta"] {
            forward.insert(key.to_owned(), Value::from(key.len()));
        }
        for key in ["delta", "gamma", "beta", "alpha"] {
            backward.insert(key.to_owned(), Value::from(key.len()));
        }
        let first = record_make(&args(serde_json::json!({ "fields": forward }))).expect("builds");
        let second = record_make(&args(serde_json::json!({ "fields": backward }))).expect("builds");
        assert_eq!(first, second);
        assert_eq!(text(&first), text(&second));
        let keys: Vec<&str> = first
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["alpha", "beta", "delta", "gamma"]);
    }

    #[test]
    fn record_make_accepts_empty_and_unusual_keys() {
        let result = record_make(&args(serde_json::json!({
            "fields": {"b c": 2, "": 1, "é": 3}
        })))
        .expect("record builds");
        let keys: Vec<&str> = result
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["", "b c", "é"]);
    }

    #[test]
    fn record_make_refuses_non_record_and_missing_fields() {
        let not_object = record_make(&args(serde_json::json!({"fields": [1]})))
            .expect_err("a list is not a record");
        assert_eq!(not_object.message, "args.fields must be an object");
        let missing = record_make(&args(serde_json::json!({}))).expect_err("fields is required");
        assert_eq!(missing.message, "args.fields is required");
    }

    #[test]
    fn hostile_values_stay_strings_and_round_trip() {
        let hostile = [
            "say \"hi\"",
            "line one\nline two\r\n",
            "{\"x\": [1, 2]}",
            "\\",
            "\u{0}\u{1f}",
            "]]}},{\"injected\":true",
            "</script><b>&amp;</b>",
        ];
        let mut fields = Map::new();
        for (index, value) in hostile.iter().enumerate() {
            fields.insert(format!("k\"{index}\n"), Value::String((*value).to_owned()));
        }
        let result = record_make(&args(serde_json::json!({ "fields": fields })))
            .expect("hostile record builds");
        for (index, value) in hostile.iter().enumerate() {
            let key = format!("k\"{index}\n");
            assert_eq!(result[&key], Value::String((*value).to_owned()));
        }
        let encoded = text(&result);
        let decoded: Value = serde_json::from_str(&encoded).expect("output is valid JSON");
        assert_eq!(decoded, result);
    }

    #[test]
    fn list_make_returns_items_unchanged() {
        let items = serde_json::json!([1, "a", {"k": "v"}, null, ["n"]]);
        let result =
            list_make(&args(serde_json::json!({ "items": items.clone() }))).expect("list builds");
        assert_eq!(result, items);
    }

    #[test]
    fn list_make_refuses_flatten_and_non_list_items() {
        let flatten = list_make(&args(serde_json::json!({"items": [1], "flatten": true})))
            .expect_err("flatten is not implemented");
        assert_eq!(flatten.message, "args.flatten is not implemented");
        let not_list = list_make(&args(serde_json::json!({"items": {"a": 1}})))
            .expect_err("a record is not a list");
        assert_eq!(not_list.message, "args.items must be an array");
    }

    #[test]
    fn record_set_adds_and_replaces_without_changing_the_input() {
        let record = serde_json::json!({"b": 1, "a": 2});
        let added = record_set(&args(serde_json::json!({
            "record": record.clone(), "key": "c", "value": "x"
        })))
        .expect("field is added");
        assert_eq!(text(&added), r#"{"a":2,"b":1,"c":"x"}"#);
        let replaced = record_set(&args(serde_json::json!({
            "record": record.clone(), "key": "a", "value": null
        })))
        .expect("field is replaced");
        assert_eq!(text(&replaced), r#"{"a":null,"b":1}"#);
        assert_eq!(record, serde_json::json!({"b": 1, "a": 2}));
    }

    #[test]
    fn record_set_keeps_hostile_values_as_data() {
        let value = "\"},{\"admin\":true,\"x\":\"\n";
        let result = record_set(&args(serde_json::json!({
            "record": {}, "key": "note", "value": value
        })))
        .expect("field is set");
        assert_eq!(result, serde_json::json!({"note": value}));
        assert_eq!(result.as_object().map(Map::len), Some(1));
    }

    #[test]
    fn record_set_refuses_bad_arguments() {
        let missing_value = record_set(&args(serde_json::json!({"record": {}, "key": "k"})))
            .expect_err("value is required");
        assert_eq!(missing_value.message, "args.value is required");
        let numeric_key = record_set(&args(serde_json::json!({
            "record": {}, "key": 5, "value": 1
        })))
        .expect_err("key must be a string");
        assert_eq!(numeric_key.message, "args.key must be a string");
        let not_record = record_set(&args(serde_json::json!({
            "record": "x", "key": "k", "value": 1
        })))
        .expect_err("record must be an object");
        assert_eq!(not_record.message, "args.record must be an object");
    }

    #[test]
    fn list_append_adds_one_or_many() {
        let one =
            list_append(&args(serde_json::json!({"list": [1], "item": 2}))).expect("one item");
        assert_eq!(one, serde_json::json!([1, 2]));
        let many = list_append(&args(serde_json::json!({"list": [1], "items": [3, 4]})))
            .expect("many items");
        assert_eq!(many, serde_json::json!([1, 3, 4]));
        let list_value = list_append(&args(serde_json::json!({"list": [1], "item": [5]})))
            .expect("a list value is one element");
        assert_eq!(list_value, serde_json::json!([1, [5]]));
        let empty = list_append(&args(serde_json::json!({"list": [], "items": []})))
            .expect("nothing to add");
        assert_eq!(empty, serde_json::json!([]));
    }

    #[test]
    fn list_append_refuses_bad_arguments() {
        let both = list_append(&args(serde_json::json!({
            "list": [], "item": 1, "items": [2]
        })))
        .expect_err("item and items cannot both be set");
        assert_eq!(both.message, "args.item and args.items cannot both be set");
        let neither = list_append(&args(serde_json::json!({"list": []})))
            .expect_err("one of item or items is required");
        assert_eq!(neither.message, "args.item is required");
        let items_not_list = list_append(&args(serde_json::json!({"list": [], "items": 1})))
            .expect_err("items must be a list");
        assert_eq!(items_not_list.message, "args.items must be an array");
        let list_not_list = list_append(&args(serde_json::json!({"list": {}, "item": 1})))
            .expect_err("list must be a list");
        assert_eq!(list_not_list.message, "args.list must be an array");
    }

    #[test]
    fn item_limit_accepts_the_bound_and_refuses_one_more() {
        // The count check is exercised directly: a real 10,000,000-item value would
        // allocate hundreds of megabytes inside a test.
        assert!(check_count(MAX_ITEMS).is_ok());
        let error = check_count(MAX_ITEMS + 1).expect_err("over the item limit");
        assert_eq!(
            error.message,
            format!("value exceeds the bounded {MAX_ITEMS}-item limit")
        );
    }

    #[test]
    fn item_limit_admits_150000_items_in_construction() {
        let items: Vec<Value> = (0..150_000).map(Value::from).collect();
        let made = list_make(&args(serde_json::json!({ "items": items.clone() })))
            .expect("150000 items build");
        assert_eq!(made.as_array().map(Vec::len), Some(150_000));
        let appended = list_append(&args(serde_json::json!({
            "list": items, "item": 1
        })))
        .expect("150001 items append");
        assert_eq!(appended.as_array().map(Vec::len), Some(150_001));
    }

    #[test]
    fn depth_limit_accepts_the_bound_and_refuses_one_more() {
        // The record is level 1 and the innermost null is level levels + 2.
        let at_limit = record_make(&args(serde_json::json!({
            "fields": {"k": nested(MAX_DEPTH - 2)}
        })));
        assert!(at_limit.is_ok(), "{at_limit:?}");
        let over = record_make(&args(serde_json::json!({
            "fields": {"k": nested(MAX_DEPTH - 1)}
        })))
        .expect_err("one level past the limit");
        assert!(over.message.contains("128-level depth limit"), "{over}");
    }

    #[test]
    fn very_deep_input_is_refused_without_recursion() {
        let error = record_set(&args(serde_json::json!({
            "record": {}, "key": "k", "value": nested(500)
        })))
        .expect_err("deep value is refused");
        assert!(error.message.contains("depth limit"), "{error}");
    }

    #[test]
    fn encoded_size_limit_is_exact() {
        // {"k":"..."} encodes to the string length plus 8 bytes.
        let fits = "x".repeat(MAX_BYTES - 8);
        let accepted = record_set(&args(serde_json::json!({
            "record": {}, "key": "k", "value": fits
        })));
        assert!(accepted.is_ok());
        let too_big = "x".repeat(MAX_BYTES - 7);
        let error = record_set(&args(serde_json::json!({
            "record": {}, "key": "k", "value": too_big
        })))
        .expect_err("one byte past the limit");
        assert!(error.message.contains("16777216-byte limit"), "{error}");
    }

    #[test]
    fn output_is_identical_across_repeated_calls() {
        let input = args(serde_json::json!({
            "fields": {"z": "last", "a": ["first"], "m": {"n": 1}}
        }));
        let first = text(&record_make(&input).expect("builds"));
        for _ in 0..3 {
            assert_eq!(text(&record_make(&input).expect("builds")), first);
        }
    }
}
