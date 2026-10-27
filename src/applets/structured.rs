//! Bounded, declarative transforms for JSON values.
//!
//! The predicates and paths accepted here are data, never executable source.
//! JSON Pointer uses RFC 6901 escaping and indexes arrays by decimal position.

use super::{absence, checkpoint_work, invalid, required_string, AppletError, ExecutionContext};
use crate::ast::Value as TypedValue;
use serde_json::{Map, Value};
use std::borrow::Cow;

const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_POINTER_BYTES: usize = 4096;
const MAX_POINTER_SEGMENTS: usize = 256;
const MAX_FILTER_CLAUSES: usize = 32;
const MAX_DEPTH: usize = 128;
const MAX_JSON_ITEMS: usize = 100_000;

pub(super) fn parse(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = match args.get("text") {
        Some(Value::String(text)) => text.as_bytes(),
        Some(_) => return Err(invalid("args.text must be a string")),
        None => input,
    };
    if text.len() > MAX_JSON_BYTES {
        return Err(invalid(format!(
            "JSON text exceeds the {MAX_JSON_BYTES}-byte limit"
        )));
    }
    let value = crate::structured::parse_json_value(text)
        .map_err(|error| invalid(format!("text is not valid JSON: {error:#}")))?;
    check_size_and_depth(&value, context)?;
    Ok(value)
}

pub(super) fn stringify(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let value = args
        .get("value")
        .ok_or_else(|| invalid("args.value is required"))?;
    let encoded_len = check_size_and_depth(value, context)?;
    let mut output = Vec::with_capacity(encoded_len);
    serde_json::to_writer(&mut output, value)
        .map_err(|error| invalid(format!("JSON value cannot be serialized: {error}")))?;
    let text = String::from_utf8(output)
        .map_err(|error| invalid(format!("JSON value cannot be serialized: {error}")))?;
    Ok(Value::String(text))
}

pub(super) fn get(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document_ref(args, input, context)?;
    let selected = get_from_pointers(document.as_ref(), args)?;
    match selected {
        Some(Value::Null) => Ok(args.get("default").cloned().unwrap_or(Value::Null)),
        Some(value) => Ok(value),
        None => args
            .get("default")
            .cloned()
            .ok_or_else(|| absence("not_found", "no JSON Pointer identifies a value")),
    }
}

/// `json.get` fast path for a parsed runtime binding. It still checks the complete
/// document's size and depth, then converts only the selected value for applet output.
pub(super) fn get_typed(
    args: &Map<String, Value>,
    document: &TypedValue,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    check_typed_size_and_depth(document, context)?;
    let pointers = get_pointers(args)?;
    let mut saw_null = false;
    for pointer in pointers {
        let selected = lookup_typed(document, &pointer)?;
        if let Some(selected) = selected {
            if matches!(selected, TypedValue::Null) {
                saw_null = true;
                continue;
            }
            return crate::structured::to_json(selected)
                .map_err(|error| invalid(format!("JSON value cannot be converted: {error}")));
        }
    }
    args.get("default")
        .cloned()
        .or_else(|| saw_null.then_some(Value::Null))
        .ok_or_else(|| absence("not_found", "no JSON Pointer identifies a value"))
}

fn get_from_pointers(
    document: &Value,
    args: &Map<String, Value>,
) -> Result<Option<Value>, AppletError> {
    let mut saw_null = false;
    for pointer in get_pointers(args)? {
        if let Some(value) = lookup(document, &pointer)? {
            if value.is_null() {
                saw_null = true;
                continue;
            }
            return Ok(Some(value.clone()));
        }
    }
    Ok(saw_null.then_some(Value::Null))
}

fn get_pointers(args: &Map<String, Value>) -> Result<Vec<Vec<String>>, AppletError> {
    if let Some(value) = args.get("pointers") {
        if args.contains_key("pointer") {
            return Err(invalid("provide pointer or pointers, not both"));
        }
        let values = value
            .as_array()
            .ok_or_else(|| invalid("args.pointers must be an array of JSON Pointer strings"))?;
        let mut pointers = Vec::with_capacity(values.len());
        for value in values {
            let pointer = value
                .as_str()
                .ok_or_else(|| invalid("args.pointers entries must be strings"))?;
            pointers.push(parse_pointer(pointer)?);
        }
        return Ok(pointers);
    }
    Ok(vec![parse_pointer(optional_pointer(args, "pointer")?)?])
}

fn check_typed_size_and_depth(
    value: &TypedValue,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    let mut pending = vec![(value, 0usize)];
    let mut work = 0usize;
    let mut bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        checkpoint_work(context, work)?;
        work += 1;
        if depth > MAX_DEPTH {
            return Err(invalid(format!(
                "JSON value exceeds the {MAX_DEPTH}-level depth limit"
            )));
        }
        bytes = bytes.saturating_add(match value {
            TypedValue::Null => 4,
            TypedValue::Bool(true) => 4,
            TypedValue::Bool(false) => 5,
            TypedValue::Integer(number) => number.to_string().len(),
            TypedValue::Number(number) => number_length(number),
            TypedValue::String(text) => escaped_length(text),
            TypedValue::List(items) => {
                for item in items {
                    checkpoint_work(context, work)?;
                    work += 1;
                    pending.push((item, depth + 1));
                }
                2 + items.len().saturating_sub(1)
            }
            TypedValue::Record(fields) => {
                let mut length = 2 + fields.len().saturating_sub(1);
                for (key, item) in fields {
                    checkpoint_work(context, work)?;
                    work += 1;
                    length = length.saturating_add(escaped_length(key) + 1);
                    pending.push((item, depth + 1));
                }
                length
            }
            TypedValue::Reference(_) => {
                return Err(invalid("JSON document contains an unresolved reference"));
            }
        });
    }
    context.checkpoint()?;
    if bytes > MAX_JSON_BYTES {
        return Err(invalid(format!(
            "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn lookup_typed<'a>(
    root: &'a TypedValue,
    pointer: &[String],
) -> Result<Option<&'a TypedValue>, AppletError> {
    let mut current = root;
    for segment in pointer {
        current = match current {
            TypedValue::Record(fields) => match fields.get(segment) {
                Some(value) => value,
                None => return Ok(None),
            },
            TypedValue::List(items) => match parse_array_index(segment) {
                Err(error) => return Err(error),
                Ok(Some(index)) => match items.get(index) {
                    Some(value) => value,
                    None => return Ok(None),
                },
                Ok(None) => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(current))
}

pub(super) fn set(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let mut document = document(args, input, context)?;
    let pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let value = args
        .get("value")
        .ok_or_else(|| invalid("args.value is required"))?;
    set_at(&mut document, &pointer, value.clone())?;
    check_size_and_depth(&document, context)?;
    Ok(document)
}

pub(super) fn map(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let array_pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let selection_pointer = parse_pointer(required_string(args, "select")?)?;
    let array = lookup(&document, &array_pointer)?
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("the selected JSON value must be an array"))?;
    let mut mapped = Vec::new();
    let mut output_bytes = 2usize;
    for (index, item) in array.iter().enumerate() {
        checkpoint_work(context, index)?;
        let value = match lookup(item, &selection_pointer)? {
            Some(Value::Null) => args.get("default").cloned().unwrap_or(Value::Null),
            Some(value) => value.clone(),
            None => args.get("default").cloned().ok_or_else(|| {
                absence(
                    "not_found",
                    format!("select pointer is absent from array item {index}"),
                )
            })?,
        };
        output_bytes = add_output_item_size(output_bytes, &value)?;
        mapped.push(value);
    }
    let result = Value::Array(mapped);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn filter(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let array_pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let array = lookup(&document, &array_pointer)?
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("the selected JSON value must be an array"))?;
    let clauses = args
        .get("where")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.where must be an array of predicate objects"))?;
    if clauses.len() > MAX_FILTER_CLAUSES {
        return Err(invalid(format!(
            "args.where exceeds the {MAX_FILTER_CLAUSES}-predicate limit"
        )));
    }
    let mut predicates = Vec::with_capacity(clauses.len());
    for (index, clause) in clauses.iter().enumerate() {
        context.checkpoint()?;
        predicates.push(parse_predicate(clause, index)?);
    }
    let mut selected = Vec::new();
    let mut output_bytes = 2usize;
    let mut work = 0usize;
    for item in array {
        checkpoint_work(context, work)?;
        work += 1;
        let mut matches = true;
        for predicate in &predicates {
            checkpoint_work(context, work)?;
            work += 1;
            if !predicate.matches(item)? {
                matches = false;
                break;
            }
        }
        if matches {
            output_bytes = add_output_item_size(output_bytes, item)?;
            selected.push(item.clone());
        }
    }
    let result = Value::Array(selected);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn keys(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let object = lookup(
        &document,
        &parse_pointer(optional_pointer(args, "pointer")?)?,
    )?
    .and_then(Value::as_object)
    .ok_or_else(|| invalid("the selected JSON value must be an object"))?;
    if object.len() > MAX_JSON_ITEMS {
        return Err(invalid("JSON object exceeds the 100000 item limit"));
    }
    let mut keys = Vec::with_capacity(object.len());
    for (index, key) in object.keys().enumerate() {
        checkpoint_work(context, index)?;
        keys.push(Value::String(key.clone()));
    }
    Ok(Value::Array(keys))
}

pub(super) fn entries(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let object = lookup(
        &document,
        &parse_pointer(optional_pointer(args, "pointer")?)?,
    )?
    .and_then(Value::as_object)
    .ok_or_else(|| invalid("the selected JSON value must be an object"))?;
    if object.len() > MAX_JSON_ITEMS {
        return Err(invalid("JSON object exceeds the 100000 item limit"));
    }
    let mut entries = Vec::with_capacity(object.len());
    for (index, (key, value)) in object.iter().enumerate() {
        checkpoint_work(context, index)?;
        entries.push(serde_json::json!({"key":key,"value":value}));
    }
    Ok(Value::Array(entries))
}

pub(super) fn has(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let pointer = parse_pointer(optional_pointer(args, "path")?)?;
    Ok(Value::Bool(lookup(&document, &pointer)?.is_some()))
}

pub(super) fn delete(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let mut document = document(args, input, context)?;
    let pointer = parse_pointer(required_string(args, "path")?)?;
    if pointer.is_empty() {
        return Err(invalid("json.delete cannot remove the document root"));
    }
    let parent = lookup_mut(&mut document, &pointer[..pointer.len() - 1])?
        .ok_or_else(|| absence("not_found", "JSON Pointer parent does not exist"))?;
    let key = pointer.last().expect("non-empty pointer");
    match parent {
        Value::Object(fields) => {
            fields
                .remove(key)
                .ok_or_else(|| absence("not_found", "JSON Pointer does not identify a value"))?;
        }
        Value::Array(items) => {
            let index =
                parse_array_index(key)?.ok_or_else(|| invalid("invalid array index for delete"))?;
            if index >= items.len() {
                return Err(absence(
                    "not_found",
                    "JSON Pointer does not identify a value",
                ));
            }
            items.remove(index);
        }
        _ => return Err(invalid("JSON Pointer parent is not an object or array")),
    }
    check_size_and_depth(&document, context)?;
    Ok(document)
}

fn lookup_mut<'a>(
    root: &'a mut Value,
    pointer: &[String],
) -> Result<Option<&'a mut Value>, AppletError> {
    let mut current = root;
    for segment in pointer {
        current = match current {
            Value::Object(fields) => match fields.get_mut(segment) {
                Some(value) => value,
                None => return Ok(None),
            },
            Value::Array(items) => match parse_array_index(segment)? {
                Some(i) => match items.get_mut(i) {
                    Some(value) => value,
                    None => return Ok(None),
                },
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(current))
}

pub(super) fn merge(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let left = args
        .get("left")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("args.left must be an object"))?;
    let right = args
        .get("right")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("args.right must be an object"))?;
    let mut merged = left.clone();
    for (key, value) in right {
        context.checkpoint()?;
        merged.insert(key.clone(), value.clone());
    }
    let result = Value::Object(merged);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn length(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let value = lookup(
        &document,
        &parse_pointer(optional_pointer(args, "pointer")?)?,
    )?
    .ok_or_else(|| absence("not_found", "JSON Pointer does not identify a value"))?;
    let length = match value {
        Value::Array(items) => items.len(),
        Value::Object(fields) => fields.len(),
        Value::String(text) => text.chars().count(),
        _ => return Err(invalid("json.length requires an array, object, or string")),
    };
    Ok(Value::Number(length.into()))
}

pub(super) fn pick(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let object = document
        .as_object()
        .ok_or_else(|| invalid("document must be an object"))?;
    let names = args
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.keys must be an array of strings"))?;
    let mut selected = Map::new();
    for (index, name) in names.iter().enumerate() {
        checkpoint_work(context, index)?;
        let key = name
            .as_str()
            .ok_or_else(|| invalid("args.keys entries must be strings"))?;
        if let Some(value) = object.get(key) {
            selected.insert(key.to_owned(), value.clone());
        }
    }
    let result = Value::Object(selected);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn values(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let object = lookup(
        &document,
        &parse_pointer(optional_pointer(args, "pointer")?)?,
    )?
    .and_then(Value::as_object)
    .ok_or_else(|| invalid("the selected JSON value must be an object"))?;
    if object.len() > MAX_JSON_ITEMS {
        return Err(invalid("JSON object exceeds the 100000 item limit"));
    }
    let mut values = Vec::with_capacity(object.len());
    for (index, value) in object.values().enumerate() {
        checkpoint_work(context, index)?;
        values.push(value.clone());
    }
    Ok(Value::Array(values))
}

pub(super) fn omit(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let object = document
        .as_object()
        .ok_or_else(|| invalid("document must be an object"))?;
    let names = args
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.keys must be an array of strings"))?;
    let mut omitted = std::collections::BTreeSet::new();
    for name in names {
        omitted.insert(
            name.as_str()
                .ok_or_else(|| invalid("args.keys entries must be strings"))?,
        );
    }
    let mut selected = Map::new();
    for (index, (key, value)) in object.iter().enumerate() {
        checkpoint_work(context, index)?;
        if !omitted.contains(key.as_str()) {
            selected.insert(key.clone(), value.clone());
        }
    }
    let result = Value::Object(selected);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn rename_keys(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let object = document
        .as_object()
        .ok_or_else(|| invalid("document must be an object"))?;
    let renames = args
        .get("renames")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("args.renames must map old key names to new string names"))?;
    let mut result = Map::new();
    for (index, (key, value)) in object.iter().enumerate() {
        checkpoint_work(context, index)?;
        let target = match renames.get(key) {
            Some(Value::String(name)) => name,
            Some(_) => return Err(invalid("args.renames values must be strings")),
            None => key,
        };
        if result.insert(target.clone(), value.clone()).is_some() {
            return Err(invalid("renaming keys would create a duplicate key"));
        }
    }
    let result = Value::Object(result);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn concat(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let left = args
        .get("left")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.left must be an array"))?;
    let right = args
        .get("right")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.right must be an array"))?;
    if left.len().saturating_add(right.len()) > 100_000 {
        return Err(invalid("concatenated array exceeds the 100000 item limit"));
    }
    let mut combined = Vec::with_capacity(left.len() + right.len());
    for (index, value) in left.iter().chain(right).enumerate() {
        checkpoint_work(context, index)?;
        combined.push(value.clone());
    }
    let result = Value::Array(combined);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn edge(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
    last: bool,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let value = lookup(
        &document,
        &parse_pointer(optional_pointer(args, "pointer")?)?,
    )?
    .ok_or_else(|| absence("not_found", "JSON Pointer does not identify a value"))?;
    let items = value
        .as_array()
        .ok_or_else(|| invalid("json.first and json.last require an array"))?;
    (if last { items.last() } else { items.first() })
        .cloned()
        .ok_or_else(|| absence("not_found", "cannot select from an empty array"))
}

pub(super) fn group_by(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let array = document
        .as_array()
        .ok_or_else(|| invalid("document must be an array"))?;
    if array.len() > MAX_JSON_ITEMS {
        return Err(invalid("JSON array exceeds the 100000 item limit"));
    }
    let pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let mut groups: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for (index, item) in array.iter().enumerate() {
        checkpoint_work(context, index)?;
        let key = lookup(item, &pointer)?
            .ok_or_else(|| absence("not_found", "group_by pointer is absent from an item"))?;
        let encoded = serde_json::to_string(key)
            .map_err(|e| invalid(format!("group key cannot be serialized: {e}")))?;
        groups.entry(encoded).or_default().push(item.clone());
    }
    let result = Value::Array(groups.into_values().map(Value::Array).collect());
    check_size_and_depth(&result, context)?;
    Ok(result)
}

pub(super) fn unique_by(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let array = document
        .as_array()
        .ok_or_else(|| invalid("document must be an array"))?;
    if array.len() > MAX_JSON_ITEMS {
        return Err(invalid("JSON array exceeds the 100000 item limit"));
    }
    let pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut output = Vec::new();
    for (index, item) in array.iter().enumerate() {
        checkpoint_work(context, index)?;
        let key = lookup(item, &pointer)?
            .ok_or_else(|| absence("not_found", "unique_by pointer is absent from an item"))?;
        let encoded = serde_json::to_string(key)
            .map_err(|e| invalid(format!("unique key cannot be serialized: {e}")))?;
        if seen.insert(encoded) {
            output.push(item.clone());
        }
    }
    Ok(Value::Array(output))
}

pub(super) fn sum(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let array = document
        .as_array()
        .ok_or_else(|| invalid("document must be an array"))?;
    let mut sum_args = Map::new();
    sum_args.insert("items".into(), Value::Array(array.clone()));
    super::aggregation::run("list.sum", &sum_args, &[], context)
}

pub(super) fn sort_by(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let array = document
        .as_array()
        .ok_or_else(|| invalid("document must be an array"))?;
    if array.len() > MAX_JSON_ITEMS {
        return Err(invalid("JSON array exceeds the 100000 item limit"));
    }
    let pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let numeric = args
        .get("numeric")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut items = array.clone();
    for item in &items {
        let key = lookup(item, &pointer)?
            .ok_or_else(|| absence("not_found", "sort_by pointer is absent from an item"))?;
        if (numeric && !key.is_number()) || (!numeric && !key.is_string()) {
            return Err(invalid(
                "sort_by keys must all be numbers with numeric=true or strings otherwise",
            ));
        }
    }
    items.sort_by(|a, b| {
        let av = lookup(a, &pointer).ok().flatten();
        let bv = lookup(b, &pointer).ok().flatten();
        match (av, bv, numeric) {
            (Some(Value::Number(a)), Some(Value::Number(b)), true) => a
                .as_f64()
                .partial_cmp(&b.as_f64())
                .unwrap_or(std::cmp::Ordering::Equal),
            (Some(Value::String(a)), Some(Value::String(b)), false) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        }
    });
    context.checkpoint()?;
    Ok(Value::Array(items))
}

fn add_output_item_size(current: usize, value: &Value) -> Result<usize, AppletError> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| invalid(format!("JSON item cannot be serialized: {error}")))?;
    let separator = usize::from(current > 2);
    let total = current.saturating_add(separator).saturating_add(counter.0);
    if total > MAX_JSON_BYTES {
        return Err(invalid(format!(
            "JSON selection exceeds the {MAX_JSON_BYTES}-byte output limit"
        )));
    }
    Ok(total)
}

fn document(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let value = if let Some(value) = args.get("document") {
        value.clone()
    } else {
        if input.len() > MAX_JSON_BYTES {
            return Err(invalid(format!(
                "JSON input exceeds the {MAX_JSON_BYTES}-byte limit"
            )));
        }
        crate::structured::parse_json_value(input)
            .map_err(|error| invalid(format!("stdin is not valid JSON: {error:#}")))?
    };
    check_size_and_depth(&value, context)?;
    Ok(value)
}

fn document_ref<'a>(
    args: &'a Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Cow<'a, Value>, AppletError> {
    context.checkpoint()?;
    if let Some(value) = args.get("document") {
        check_size_and_depth(value, context)?;
        return Ok(Cow::Borrowed(value));
    }
    if input.len() > MAX_JSON_BYTES {
        return Err(invalid(format!(
            "JSON input exceeds the {MAX_JSON_BYTES}-byte limit"
        )));
    }
    let value = crate::structured::parse_json_value(input)
        .map_err(|error| invalid(format!("stdin is not valid JSON: {error:#}")))?;
    check_size_and_depth(&value, context)?;
    Ok(Cow::Owned(value))
}

fn check_size_and_depth(
    value: &Value,
    context: &ExecutionContext<'_>,
) -> Result<usize, AppletError> {
    // One walk checks the depth and adds up the exact length of the compact JSON
    // encoding, so the size limit needs no serialization pass over the document.
    let mut pending = vec![(value, 0usize)];
    let mut work = 0usize;
    let mut bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        checkpoint_work(context, work)?;
        work += 1;
        if depth > MAX_DEPTH {
            return Err(invalid(format!(
                "JSON value exceeds the {MAX_DEPTH}-level depth limit"
            )));
        }
        bytes = bytes.saturating_add(match value {
            Value::Null => 4,
            Value::Bool(true) => 4,
            Value::Bool(false) => 5,
            Value::Number(number) => number_length(number),
            Value::String(text) => escaped_length(text),
            Value::Array(items) => {
                for item in items {
                    checkpoint_work(context, work)?;
                    work += 1;
                    pending.push((item, depth + 1));
                }
                // Brackets and the commas between items.
                2 + items.len().saturating_sub(1)
            }
            Value::Object(fields) => {
                let mut length = 2 + fields.len().saturating_sub(1);
                for (key, item) in fields {
                    checkpoint_work(context, work)?;
                    work += 1;
                    // The quoted key and its colon.
                    length = length.saturating_add(escaped_length(key) + 1);
                    pending.push((item, depth + 1));
                }
                length
            }
        });
    }
    context.checkpoint()?;
    if bytes > MAX_JSON_BYTES {
        return Err(invalid(format!(
            "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
        )));
    }
    Ok(bytes)
}

/// The length of `text` as a quoted JSON string in serde_json's compact encoding:
/// `"` and `\` and the short control escapes take two bytes, other control bytes take
/// six (`\u00XX`), and every other byte, including non-ASCII UTF-8, is copied.
fn escaped_length(text: &str) -> usize {
    let escapes: usize = text
        .bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 1,
            0x00..=0x1f => 5,
            _ => 0,
        })
        .sum();
    text.len() + escapes + 2
}

/// The length of a number as serde_json writes it (integers in decimal, floats in
/// the shortest round-trip form), which is its `Display` form.
fn number_length(number: &serde_json::Number) -> usize {
    use std::fmt::Write;
    struct Counter(usize);
    impl Write for Counter {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0 += text.len();
            Ok(())
        }
    }
    let mut counter = Counter(0);
    // Writing to a counter cannot fail.
    let _ = write!(counter, "{number}");
    counter.0
}

fn optional_pointer<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, AppletError> {
    match args.get(name) {
        None => Ok(""),
        Some(Value::String(pointer)) => Ok(pointer),
        Some(_) => Err(invalid(format!("args.{name} must be a string"))),
    }
}

fn parse_pointer(pointer: &str) -> Result<Vec<String>, AppletError> {
    if pointer.len() > MAX_POINTER_BYTES {
        return Err(invalid(format!(
            "JSON Pointer exceeds the {MAX_POINTER_BYTES}-byte limit"
        )));
    }
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(invalid("JSON Pointer must be empty or start with '/'"));
    };
    let mut segments = Vec::new();
    for raw in rest.split('/') {
        let mut decoded = String::new();
        let mut chars = raw.chars();
        while let Some(character) = chars.next() {
            if character != '~' {
                decoded.push(character);
                continue;
            }
            match chars.next() {
                Some('0') => decoded.push('~'),
                Some('1') => decoded.push('/'),
                _ => return Err(invalid("JSON Pointer contains an invalid '~' escape")),
            }
        }
        segments.push(decoded);
        if segments.len() > MAX_POINTER_SEGMENTS {
            return Err(invalid(format!(
                "JSON Pointer exceeds the {MAX_POINTER_SEGMENTS}-segment limit"
            )));
        }
    }
    Ok(segments)
}

fn lookup<'a>(root: &'a Value, pointer: &[String]) -> Result<Option<&'a Value>, AppletError> {
    let mut current = root;
    for segment in pointer {
        current = match current {
            Value::Object(fields) => match fields.get(segment) {
                Some(value) => value,
                None => return Ok(None),
            },
            // A name on an array is absent; a malformed number (`01`) is an error.
            Value::Array(items) => match parse_array_index(segment) {
                Err(error)
                    if !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()) =>
                {
                    return Err(error)
                }
                Err(_) => return Ok(None),
                Ok(Some(index)) => match items.get(index) {
                    Some(value) => value,
                    None => return Ok(None),
                },
                Ok(None) => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(current))
}

fn parse_array_index(segment: &str) -> Result<Option<usize>, AppletError> {
    if segment == "-" {
        return Ok(None);
    }
    if segment.is_empty()
        || (segment.len() > 1 && segment.starts_with('0'))
        || !segment.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(format!(
            "invalid JSON Pointer array index {segment:?}"
        )));
    }
    segment.parse::<usize>().map(Some).map_err(|_| {
        invalid(format!(
            "JSON Pointer array index is too large: {segment:?}"
        ))
    })
}

fn set_at(root: &mut Value, pointer: &[String], replacement: Value) -> Result<(), AppletError> {
    if pointer.is_empty() {
        *root = replacement;
        return Ok(());
    }
    let mut current = root;
    for segment in &pointer[..pointer.len() - 1] {
        current = match current {
            Value::Object(fields) => fields.get_mut(segment).ok_or_else(|| {
                absence(
                    "not_found",
                    format!("JSON Pointer parent {segment:?} is absent"),
                )
            })?,
            Value::Array(items) => {
                let index = parse_array_index(segment)?.ok_or_else(|| {
                    invalid("'-' is only valid as the final JSON Pointer segment")
                })?;
                items.get_mut(index).ok_or_else(|| {
                    absence(
                        "not_found",
                        format!("JSON Pointer array index {index} is absent"),
                    )
                })?
            }
            _ => return Err(invalid("JSON Pointer parent is not an object or array")),
        };
    }
    let final_segment = pointer.last().expect("non-empty pointer");
    match current {
        Value::Object(fields) => {
            fields.insert(final_segment.clone(), replacement);
        }
        Value::Array(items) => {
            if final_segment == "-" {
                items.push(replacement);
            } else {
                let index = parse_array_index(final_segment)?
                    .ok_or_else(|| invalid("invalid final JSON Pointer array index"))?;
                if index == items.len() {
                    items.push(replacement);
                } else if let Some(item) = items.get_mut(index) {
                    *item = replacement;
                } else {
                    return Err(absence(
                        "not_found",
                        format!("JSON Pointer array index {index} is absent"),
                    ));
                }
            }
        }
        _ => return Err(invalid("JSON Pointer parent is not an object or array")),
    }
    Ok(())
}

#[derive(Debug)]
enum Predicate {
    Exists(Vec<String>, bool),
    Equals(Vec<String>, Value),
    NotEquals(Vec<String>, Value),
    Type(Vec<String>, String),
}

impl Predicate {
    fn matches(&self, item: &Value) -> Result<bool, AppletError> {
        Ok(match self {
            Self::Exists(pointer, expected) => lookup(item, pointer)?.is_some() == *expected,
            Self::Equals(pointer, expected) => {
                lookup(item, pointer)?.is_some_and(|actual| json_values_equal(actual, expected))
            }
            // A missing value is not equal to any JSON value. This also means
            // an absent nested field naturally passes a `not_equals` filter.
            Self::NotEquals(pointer, expected) => {
                lookup(item, pointer)?.is_none_or(|actual| !json_values_equal(actual, expected))
            }
            Self::Type(pointer, expected) => {
                lookup(item, pointer)?.is_some_and(|value| json_type(value) == expected)
            }
        })
    }
}

fn json_values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            match (integer_value(left), integer_value(right)) {
                (Some(left), Some(right)) => left == right,
                (Some(integer), None) => float_matches_integer(right.as_f64(), integer),
                (None, Some(integer)) => float_matches_integer(left.as_f64(), integer),
                (None, None) => left.as_f64() == right.as_f64(),
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_values_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| json_values_equal(value, other))
                })
        }
        _ => left == right,
    }
}

fn integer_value(number: &serde_json::Number) -> Option<i128> {
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

fn float_matches_integer(number: Option<f64>, integer: i128) -> bool {
    let Some(number) = number else {
        return false;
    };
    // JSON integer values fit in i64/u64. Reject rounded f64s at either bound
    // before casting, then compare the integral f64 value without lossy integer
    // to-float conversion.
    number.fract() == 0.0
        && number >= i64::MIN as f64
        && number < 18_446_744_073_709_551_616.0
        && number as i128 == integer
}

fn parse_predicate(value: &Value, index: usize) -> Result<Predicate, AppletError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid(format!("args.where[{index}] must be a predicate object")))?;
    let pointer = match object.get("pointer") {
        None => Vec::new(),
        Some(Value::String(pointer)) => parse_pointer(pointer)?,
        Some(_) => {
            return Err(invalid(format!(
                "args.where[{index}].pointer must be a string"
            )));
        }
    };
    let tests = ["exists", "equals", "not_equals", "type"]
        .into_iter()
        .filter(|field| object.contains_key(*field))
        .collect::<Vec<_>>();
    if tests.len() != 1 || object.len() != tests.len() + usize::from(object.contains_key("pointer"))
    {
        return Err(invalid(format!(
            "args.where[{index}] must contain pointer and exactly one of exists, equals, not_equals, or type"
        )));
    }
    match tests[0] {
        "exists" => object
            .get("exists")
            .and_then(Value::as_bool)
            .map(|expected| Predicate::Exists(pointer, expected))
            .ok_or_else(|| invalid(format!("args.where[{index}].exists must be a boolean"))),
        "equals" => Ok(Predicate::Equals(
            pointer,
            object
                .get("equals")
                .expect("selected existing field")
                .clone(),
        )),
        "not_equals" => Ok(Predicate::NotEquals(
            pointer,
            object
                .get("not_equals")
                .expect("selected existing field")
                .clone(),
        )),
        "type" => {
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(format!("args.where[{index}].type must be a string")))?;
            if !["null", "boolean", "number", "string", "array", "object"].contains(&kind) {
                return Err(invalid(format!(
                    "args.where[{index}].type must be null, boolean, number, string, array, or object"
                )));
            }
            Ok(Predicate::Type(pointer, kind.to_owned()))
        }
        _ => unreachable!("predicate fields are selected from the fixed list"),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_get_matches_the_json_value_path() {
        let source = br#"{"a":[{"x":1},null],"meta":{"count":5000}}"#;
        let typed = crate::structured::parse(source).expect("typed JSON parses");
        let json: Value = serde_json::from_slice(source).expect("JSON parses");
        let args = Map::from_iter([(
            "pointer".to_owned(),
            Value::String("/meta/count".to_owned()),
        )]);
        let context = ExecutionContext::new(&|| Ok(()));

        assert_eq!(
            get_typed(&args, &typed, &context).expect("typed lookup succeeds"),
            get(
                &Map::from_iter([
                    ("document".to_owned(), json),
                    (
                        "pointer".to_owned(),
                        Value::String("/meta/count".to_owned()),
                    ),
                ]),
                &[],
                &context
            )
            .expect("JSON value lookup succeeds")
        );
    }

    #[test]
    fn typed_get_preserves_size_and_depth_failures() {
        let context = ExecutionContext::new(&|| Ok(()));
        let args = Map::from_iter([("pointer".to_owned(), Value::String(String::new()))]);
        let too_deep =
            (0..=MAX_DEPTH).fold(TypedValue::Null, |value, _| TypedValue::List(vec![value]));
        assert_eq!(
            get_typed(&args, &too_deep, &context)
                .expect_err("deep JSON is rejected")
                .to_string(),
            invalid(format!(
                "JSON value exceeds the {MAX_DEPTH}-level depth limit"
            ))
            .to_string()
        );

        let oversized = TypedValue::String("x".repeat(MAX_JSON_BYTES));
        assert_eq!(
            get_typed(&args, &oversized, &context)
                .expect_err("oversized JSON is rejected")
                .to_string(),
            invalid(format!(
                "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
            ))
            .to_string()
        );
    }

    /// The walked length must equal serde_json's compact encoding for every value kind,
    /// including escapes, non-ASCII text and every number form.
    #[test]
    fn walked_length_equals_the_compact_encoding() {
        let values = [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"a": [1, -2, 3.5, 1e300, 0.1, 18446744073709551615u64, false]}),
            serde_json::json!({"quote\"key": "tab\t nl\n cr\r bell\u{7} del\u{7f} \\ \u{8}\u{c} é 日本"}),
            serde_json::json!([[[{"x": {"y": [null, "", "\u{0}\u{1f}"]}}]]]),
            serde_json::json!({"meta": {"count": 5000}, "items": [{"id": 1, "tags": ["a", "b"]}]}),
        ];
        for value in values {
            let mut pending = vec![&value];
            let mut walked = 0usize;
            while let Some(value) = pending.pop() {
                walked += match value {
                    Value::Null => 4,
                    Value::Bool(true) => 4,
                    Value::Bool(false) => 5,
                    Value::Number(number) => number_length(number),
                    Value::String(text) => escaped_length(text),
                    Value::Array(items) => {
                        pending.extend(items);
                        2 + items.len().saturating_sub(1)
                    }
                    Value::Object(fields) => {
                        pending.extend(fields.values());
                        2 + fields.len().saturating_sub(1)
                            + fields
                                .keys()
                                .map(|key| escaped_length(key) + 1)
                                .sum::<usize>()
                    }
                };
            }
            assert_eq!(walked, serde_json::to_vec(&value).unwrap().len(), "{value}");
        }
    }

    #[test]
    fn stringify_reserves_the_checked_compact_length() {
        let context = ExecutionContext::new(&|| Ok(()));
        let value = serde_json::json!({
            "quotes": "\"\\",
            "control": "\u{1f}",
            "items": [1, 2, 3],
            "large": "x".repeat(10_000),
        });
        let args = Map::from_iter([("value".into(), value.clone())]);
        let Value::String(text) = stringify(&args, &context).expect("stringify succeeds") else {
            panic!("stringify returns text");
        };
        assert_eq!(text, serde_json::to_string(&value).unwrap());
        assert_eq!(text.len(), check_size_and_depth(&value, &context).unwrap());
        assert_eq!(text.capacity(), text.len());
    }
}
