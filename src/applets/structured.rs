//! Bounded, declarative transforms for JSON values.
//!
//! The predicates and paths accepted here are data, never executable source.
//! JSON Pointer uses RFC 6901 escaping and indexes arrays by decimal position.

#[cfg(test)]
use super::shared::escaped_length;
use super::shared::{check_size_and_depth, escaped_length_checked, number_length};
use super::{
    absence, checkpoint_work, invalid, optional_bool, required_string, AppletError,
    ExecutionContext,
};
use crate::ast::Value as TypedValue;
use serde_json::{Map, Value};
use std::borrow::Cow;

const MAX_JSON_BYTES: usize = super::shared::MAX_BYTES;
const MAX_FILTER_CLAUSES: usize = 32;
const MAX_DEPTH: usize = super::shared::MAX_DEPTH;
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
        Some(Value::Null) => Ok(checked_default(args, context)?.unwrap_or(Value::Null)),
        Some(value) => Ok(value),
        None => checked_default(args, context)?
            .ok_or_else(|| absence("not_found", "no JSON Pointer identifies a value")),
    }
}

/// A returned `args.default` becomes applet output, so it must satisfy the same
/// size and depth limits as a value selected from a checked document.
fn checked_default(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Option<Value>, AppletError> {
    checked_default_value(args.get("default"), context)
}

fn checked_default_value(
    default: Option<&Value>,
    context: &ExecutionContext<'_>,
) -> Result<Option<Value>, AppletError> {
    let Some(value) = default else {
        return Ok(None);
    };
    check_size_and_depth(value, context)?;
    Ok(Some(value.clone()))
}

/// `json.get` of one `pointer` over JSON `bytes` without building the whole typed tree.
/// The result equals `parse` followed by `get_typed` with `{pointer, default}`, including
/// error text and the order in which errors are reported: parse errors first, then the
/// document's size and depth, then the pointer, then the lookup and the default.
/// Only the single pointer form is handled; `pointers` still goes through `get_typed`.
// Not wired: the runtime has no single-use liveness for JSON bindings (see NEEDS).
#[allow(dead_code)]
pub(crate) fn get_while_parsing(
    bytes: &[u8],
    pointer: &str,
    default: Option<&Value>,
    context: &ExecutionContext<'_>,
) -> anyhow::Result<Result<Value, AppletError>> {
    // A malformed pointer is reported only after the document has been checked, so the
    // walk runs with no segments in that case and its selection is ignored.
    let segments = parse_pointer(pointer);
    let walk = crate::structured::walk_pointer(bytes, segments.as_deref().unwrap_or(&[]))?;
    Ok(finish_while_parsing(walk, segments, default, context))
}

fn finish_while_parsing(
    walk: crate::structured::PointerWalk,
    segments: Result<Vec<String>, AppletError>,
    default: Option<&Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    use crate::structured::PointerSelection;
    context.checkpoint()?;
    if walk.max_depth > MAX_DEPTH {
        return Err(invalid(format!(
            "JSON value exceeds the {MAX_DEPTH}-level depth limit"
        )));
    }
    if walk.encoded_bytes > MAX_JSON_BYTES {
        return Err(invalid(format!(
            "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
        )));
    }
    segments?;
    match walk.selected {
        PointerSelection::Failed(message) => Err(invalid(message)),
        PointerSelection::Found(Value::Null) => {
            Ok(checked_default_value(default, context)?.unwrap_or(Value::Null))
        }
        PointerSelection::Found(value) => Ok(value),
        PointerSelection::Absent => checked_default_value(default, context)?
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
            return typed_value_json(selected);
        }
    }
    checked_default(args, context)?
        .or_else(|| saw_null.then_some(Value::Null))
        .ok_or_else(|| absence("not_found", "no JSON Pointer identifies a value"))
}

/// Convert one value selected from a checked typed document. Such a value holds no
/// reference, so the conversion cannot fail in practice; the error keeps the result typed.
fn typed_value_json(value: &TypedValue) -> Result<Value, AppletError> {
    crate::structured::to_json(value)
        .map_err(|error| invalid(format!("JSON value cannot be converted: {error}")))
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
            TypedValue::String(text) => escaped_length_checked(text, context)?,
            TypedValue::List(items) => {
                let minimum = 2usize
                    .saturating_add(items.len().saturating_sub(1))
                    .saturating_add(items.len());
                if bytes.saturating_add(pending.len()).saturating_add(minimum) > MAX_JSON_BYTES {
                    return Err(invalid(format!(
                        "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
                    )));
                }
                for item in items {
                    checkpoint_work(context, work)?;
                    work += 1;
                    pending.push((item, depth + 1));
                }
                2 + items.len().saturating_sub(1)
            }
            TypedValue::Record(fields) => {
                let mut length = 2 + fields.len().saturating_sub(1);
                if bytes
                    .saturating_add(pending.len())
                    .saturating_add(length)
                    .saturating_add(fields.len().saturating_mul(4))
                    > MAX_JSON_BYTES
                {
                    return Err(invalid(format!(
                        "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
                    )));
                }
                for (key, item) in fields {
                    checkpoint_work(context, work)?;
                    work += 1;
                    length = length.saturating_add(escaped_length_checked(key, context)? + 1);
                    if bytes
                        .saturating_add(length)
                        .saturating_add(pending.len())
                        .saturating_add(1)
                        > MAX_JSON_BYTES
                    {
                        return Err(invalid(format!(
                            "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
                        )));
                    }
                    pending.push((item, depth + 1));
                }
                length
            }
            TypedValue::Reference(_) => {
                return Err(invalid("JSON document contains an unresolved reference"));
            }
        });
        if bytes.saturating_add(pending.len()) > MAX_JSON_BYTES {
            return Err(invalid(format!(
                "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
            )));
        }
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
            // Mirrors `lookup`: a name on an array is absent; a malformed number is an error.
            TypedValue::List(items) => match parse_array_index(segment) {
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
    // Borrow the document and clone only the selected values, so a bound document
    // is not copied as a whole before the selection runs.
    let document = document_ref(args, input, context)?;
    let array_pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let selection_pointer = parse_pointer(required_string(args, "select")?)?;
    let array = match lookup(&document, &array_pointer)? {
        Some(Value::Array(array)) => array,
        _ => return Err(invalid("the selected JSON value must be an array")),
    };
    let mut mapped = Vec::new();
    let mut output_bytes = 2usize;
    for (index, item) in array.iter().enumerate() {
        checkpoint_work(context, index)?;
        let value = match lookup(item, &selection_pointer)? {
            Some(Value::Null) => mapped_default(args),
            Some(value) => value.clone(),
            None => mapped_absent(args, index)?,
        };
        output_bytes = add_output_item_size(output_bytes, &value)?;
        mapped.push(value);
    }
    let result = Value::Array(mapped);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

/// `json.map` over a bound document, read in place from the typed tree. Only each
/// selected value is converted, so the result, the errors and their order equal `map`
/// over the converted document.
pub(super) fn map_typed(
    args: &Map<String, Value>,
    document: &TypedValue,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    check_typed_size_and_depth(document, context)?;
    let array_pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let selection_pointer = parse_pointer(required_string(args, "select")?)?;
    let items = match lookup_typed(document, &array_pointer)? {
        Some(TypedValue::List(items)) => items,
        _ => return Err(invalid("the selected JSON value must be an array")),
    };
    let mut mapped = Vec::new();
    let mut output_bytes = 2usize;
    for (index, item) in items.iter().enumerate() {
        checkpoint_work(context, index)?;
        let value = match lookup_typed(item, &selection_pointer)? {
            Some(TypedValue::Null) => mapped_default(args),
            Some(value) => typed_value_json(value)?,
            None => mapped_absent(args, index)?,
        };
        output_bytes = add_output_item_size(output_bytes, &value)?;
        mapped.push(value);
    }
    let result = Value::Array(mapped);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

/// The value `json.map` yields for a selected `null`.
fn mapped_default(args: &Map<String, Value>) -> Value {
    args.get("default").cloned().unwrap_or(Value::Null)
}

/// The value `json.map` yields for an absent selection, or the error when no default is set.
fn mapped_absent(args: &Map<String, Value>, index: usize) -> Result<Value, AppletError> {
    args.get("default").cloned().ok_or_else(|| {
        absence(
            "not_found",
            format!("select pointer is absent from array item {index}"),
        )
    })
}

pub(super) fn filter(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    // Borrow the document; only matching items are cloned into the result.
    let document = document_ref(args, input, context)?;
    let array_pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let Some(array) = lookup(&document, &array_pointer)?.and_then(Value::as_array) else {
        return Err(invalid("the selected JSON value must be an array"));
    };
    let predicates = parse_where(args, context)?;
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
            if !predicate.matches(item, context, &mut work)? {
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

/// `json.filter` over a bound document, read in place from the typed tree. Only matching
/// items are converted into the result; the errors and their order equal `filter` over
/// the converted document.
pub(super) fn filter_typed(
    args: &Map<String, Value>,
    document: &TypedValue,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    check_typed_size_and_depth(document, context)?;
    let array_pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    let Some(TypedValue::List(items)) = lookup_typed(document, &array_pointer)? else {
        return Err(invalid("the selected JSON value must be an array"));
    };
    let predicates = parse_where(args, context)?;
    let mut selected = Vec::new();
    let mut output_bytes = 2usize;
    let mut work = 0usize;
    for item in items {
        checkpoint_work(context, work)?;
        work += 1;
        let mut matches = true;
        for predicate in &predicates {
            checkpoint_work(context, work)?;
            work += 1;
            if !predicate.matches_typed(item, context, &mut work)? {
                matches = false;
                break;
            }
        }
        if matches {
            let value = typed_value_json(item)?;
            output_bytes = add_output_item_size(output_bytes, &value)?;
            selected.push(value);
        }
    }
    let result = Value::Array(selected);
    check_size_and_depth(&result, context)?;
    Ok(result)
}

/// The `where` clauses of `json.filter`, checked and parsed in order.
fn parse_where(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Vec<Predicate>, AppletError> {
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
        check_size_and_depth(clause, context)?;
        predicates.push(parse_predicate(clause, index)?);
    }
    Ok(predicates)
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
    // Wrapping each value in an entry object makes the output deeper and
    // larger than the checked input, so check the transformed result.
    let result = Value::Array(entries);
    check_size_and_depth(&result, context)?;
    Ok(result)
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
    // Keep the existing serialized-key group order using the first representative.
    let mut groups: std::collections::HashMap<Vec<u8>, (String, Vec<Value>)> =
        std::collections::HashMap::new();
    for (index, item) in array.iter().enumerate() {
        checkpoint_work(context, index)?;
        let key = lookup(item, &pointer)?
            .ok_or_else(|| absence("not_found", "group_by pointer is absent from an item"))?;
        let encoded = super::shared::equality_key(key, context)?;
        if let Some((_, items)) = groups.get_mut(&encoded) {
            items.push(item.clone());
        } else {
            let order = serde_json::to_string(key)
                .map_err(|e| invalid(format!("group key cannot be serialized: {e}")))?;
            groups.insert(encoded, (order, vec![item.clone()]));
        }
    }
    let mut groups: Vec<_> = groups.into_values().collect();
    let mut comparisons = 0usize;
    let mut failure = None;
    groups.sort_unstable_by(|a, b| {
        if failure.is_none() {
            if let Err(error) = checkpoint_work(context, comparisons) {
                failure = Some(error);
            }
            comparisons += 1;
        }
        a.0.cmp(&b.0)
    });
    if let Some(error) = failure {
        return Err(error);
    }
    let result = Value::Array(
        groups
            .into_iter()
            .map(|(_, items)| Value::Array(items))
            .collect(),
    );
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
        let encoded = super::shared::equality_key(key, context)?;
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
    let numeric = optional_bool(args, "numeric", false)?;
    let mut items = array.clone();
    for (index, item) in items.iter().enumerate() {
        checkpoint_work(context, index)?;
        let key = lookup(item, &pointer)?
            .ok_or_else(|| absence("not_found", "sort_by pointer is absent from an item"))?;
        if (numeric && !key.is_number()) || (!numeric && !key.is_string()) {
            return Err(invalid(
                "sort_by keys must all be numbers with numeric=true or strings otherwise",
            ));
        }
    }
    let comparisons = std::cell::Cell::new(0usize);
    let failure = std::cell::RefCell::new(None);
    items.sort_by(|a, b| {
        if failure.borrow().is_none() {
            let work = comparisons.get();
            comparisons.set(work + 1);
            if let Err(error) = checkpoint_work(context, work) {
                *failure.borrow_mut() = Some(error);
            }
        }
        let av = lookup(a, &pointer).ok().flatten();
        let bv = lookup(b, &pointer).ok().flatten();
        match (av, bv, numeric) {
            (Some(Value::Number(a)), Some(Value::Number(b)), true) => {
                super::shared::numeric_order(a, b).unwrap_or(std::cmp::Ordering::Equal)
            }
            (Some(Value::String(a)), Some(Value::String(b)), false) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        }
    });
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
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
    if let Some(value) = args.get("document") {
        check_size_and_depth(value, context)?;
        return Ok(value.clone());
    }
    let value = {
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

fn optional_pointer<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, AppletError> {
    match args.get(name) {
        None => Ok(""),
        Some(Value::String(pointer)) => Ok(pointer),
        Some(_) => Err(invalid(format!("args.{name} must be a string"))),
    }
}

fn parse_pointer(pointer: &str) -> Result<Vec<String>, AppletError> {
    if pointer.len() > crate::contracts::JSON_POINTER_MAX_BYTES {
        return Err(invalid(format!(
            "JSON Pointer exceeds the {}-byte limit",
            crate::contracts::JSON_POINTER_MAX_BYTES
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
        if segments.len() > crate::contracts::JSON_POINTER_MAX_SEGMENTS {
            return Err(invalid(format!(
                "JSON Pointer exceeds the {}-segment limit",
                crate::contracts::JSON_POINTER_MAX_SEGMENTS
            )));
        }
    }
    Ok(segments)
}

pub(super) fn validate_pointer(pointer: &str) -> Result<(), AppletError> {
    parse_pointer(pointer).map(|_| ())
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
    fn matches(
        &self,
        item: &Value,
        context: &ExecutionContext<'_>,
        work: &mut usize,
    ) -> Result<bool, AppletError> {
        Ok(match self {
            Self::Exists(pointer, expected) => lookup(item, pointer)?.is_some() == *expected,
            Self::Equals(pointer, expected) => match lookup(item, pointer)? {
                Some(actual) => super::shared::json_equal(actual, expected, context, work)?,
                None => false,
            },
            // A missing value is not equal to any JSON value. This also means
            // an absent nested field naturally passes a `not_equals` filter.
            Self::NotEquals(pointer, expected) => match lookup(item, pointer)? {
                Some(actual) => !super::shared::json_equal(actual, expected, context, work)?,
                None => true,
            },
            Self::Type(pointer, expected) => {
                lookup(item, pointer)?.is_some_and(|value| json_type(value) == expected)
            }
        })
    }

    /// `matches` for an item of a bound document. The same lookups and comparisons run
    /// on the typed tree; a selected value is converted only where equality needs it.
    fn matches_typed(
        &self,
        item: &TypedValue,
        context: &ExecutionContext<'_>,
        work: &mut usize,
    ) -> Result<bool, AppletError> {
        Ok(match self {
            Self::Exists(pointer, expected) => lookup_typed(item, pointer)?.is_some() == *expected,
            Self::Equals(pointer, expected) => match lookup_typed(item, pointer)? {
                Some(actual) => {
                    let actual = typed_value_json(actual)?;
                    super::shared::json_equal(&actual, expected, context, work)?
                }
                None => false,
            },
            Self::NotEquals(pointer, expected) => match lookup_typed(item, pointer)? {
                Some(actual) => {
                    let actual = typed_value_json(actual)?;
                    !super::shared::json_equal(&actual, expected, context, work)?
                }
                None => true,
            },
            Self::Type(pointer, expected) => {
                lookup_typed(item, pointer)?.is_some_and(|value| typed_json_type(value) == expected)
            }
        })
    }
}

/// The JSON type name `json_type` gives the converted value of a typed node.
fn typed_json_type(value: &TypedValue) -> &'static str {
    match value {
        TypedValue::Null => "null",
        TypedValue::Bool(_) => "boolean",
        TypedValue::Integer(_) | TypedValue::Number(_) => "number",
        TypedValue::String(_) => "string",
        TypedValue::List(_) => "array",
        TypedValue::Record(_) => "object",
        // A checked document holds no reference; this name matches no predicate type.
        TypedValue::Reference(_) => "reference",
    }
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
    fn json_pointer_parser_accepts_runtime_limits_and_rejects_the_next_byte_or_segment() {
        let at_byte_limit = format!(
            "/{}",
            "x".repeat(crate::contracts::JSON_POINTER_MAX_BYTES - 1)
        );
        assert_eq!(
            at_byte_limit.len(),
            crate::contracts::JSON_POINTER_MAX_BYTES
        );
        assert!(parse_pointer(&at_byte_limit).is_ok());
        let over_byte_limit = format!("{at_byte_limit}x");
        assert!(parse_pointer(&over_byte_limit)
            .unwrap_err()
            .message
            .contains("4096-byte limit"));

        let at_segment_limit = "/x".repeat(crate::contracts::JSON_POINTER_MAX_SEGMENTS);
        assert_eq!(
            parse_pointer(&at_segment_limit).unwrap().len(),
            crate::contracts::JSON_POINTER_MAX_SEGMENTS
        );
        let over_segment_limit = format!("{at_segment_limit}/x");
        assert!(parse_pointer(&over_segment_limit)
            .unwrap_err()
            .message
            .contains("256-segment limit"));
    }

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

    #[test]
    fn typed_get_treats_nonnumeric_array_segments_as_absent_like_json_get() {
        let source = br#"{"x":[1,2]}"#;
        let typed = crate::structured::parse(source).expect("typed JSON parses");
        let json: Value = serde_json::from_slice(source).expect("JSON parses");
        let context = ExecutionContext::new(&|| Ok(()));
        let fallback = Value::String("fallback".to_owned());

        for pointer in ["/x/name", "/x/-", "/x/", "/x/01", "/x/1", "/x/5"] {
            for default in [None, Some(fallback.clone())] {
                let mut args =
                    Map::from_iter([("pointer".to_owned(), Value::String(pointer.to_owned()))]);
                if let Some(default) = &default {
                    args.insert("default".to_owned(), default.clone());
                }
                let mut json_args = args.clone();
                json_args.insert("document".to_owned(), json.clone());

                let typed_result = get_typed(&args, &typed, &context).map_err(|e| e.to_string());
                let json_result = get(&json_args, &[], &context).map_err(|e| e.to_string());
                assert_eq!(
                    typed_result, json_result,
                    "typed and JSON lookups differ for pointer {pointer:?} with default {default:?}"
                );
            }
        }

        let with_fallback = Map::from_iter([
            ("pointer".to_owned(), Value::String("/x/name".to_owned())),
            ("default".to_owned(), fallback.clone()),
        ]);
        assert_eq!(
            get_typed(&with_fallback, &typed, &context).expect("fallback is returned"),
            fallback
        );
        let without_fallback =
            Map::from_iter([("pointer".to_owned(), Value::String("/x/name".to_owned()))]);
        assert!(get_typed(&without_fallback, &typed, &context).is_err());
        let malformed = Map::from_iter([("pointer".to_owned(), Value::String("/x/01".to_owned()))]);
        assert!(get_typed(&malformed, &typed, &context).is_err());
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

    /// The vector escape counter must agree with the scalar counter and with
    /// serde_json's compact encoding for every byte class, UTF-8 text and the
    /// lengths around the vector threshold and the block sizes.
    #[test]
    fn escaped_length_vector_path_matches_scalar_and_serde_json() {
        use super::super::shared::{escape_extra_vector, ESCAPE_VECTOR_THRESHOLD};

        fn scalar_extra(bytes: &[u8]) -> usize {
            bytes
                .iter()
                .map(|byte| match byte {
                    b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 1,
                    0x00..=0x1f => 5,
                    _ => 0,
                })
                .sum()
        }

        // Every byte class: all ASCII bytes plus multi-byte UTF-8, padded so
        // the vector path engages.
        let mut classes: Vec<u8> = (0x00u8..=0x7fu8).collect();
        classes.extend_from_slice("é 日本\"\\\u{8}\t\n\u{c}\r\u{0}\u{1f}".as_bytes());
        let mut long_classes = classes.clone();
        while long_classes.len() < ESCAPE_VECTOR_THRESHOLD + 33 {
            long_classes.extend_from_slice(&classes);
        }
        // Boundary lengths around the threshold and both block sizes.
        let mut cases = vec![
            String::from(""),
            String::from("\"\\\u{8}\t\n\u{c}\r\u{0}\u{1f}é"),
            "a".repeat(ESCAPE_VECTOR_THRESHOLD - 1),
            "b".repeat(ESCAPE_VECTOR_THRESHOLD),
            "c".repeat(ESCAPE_VECTOR_THRESHOLD + 1),
            "d".repeat(31),
            "e".repeat(32),
            "f".repeat(33),
            "g".repeat(63),
            "h".repeat(64),
            "i".repeat(65),
        ];
        cases.push(String::from_utf8(long_classes).unwrap());
        // A deterministic long mix of quotes, backslashes, controls and UTF-8.
        let mut mixed = String::new();
        for index in 0..4096 {
            mixed.push(match index % 16 {
                0 => '"',
                1 => '\\',
                2 => '\u{8}',
                3 => '\t',
                4 => '\n',
                5 => '\u{c}',
                6 => '\r',
                7 => '\u{0}',
                8 => '\u{1f}',
                9 => 'é',
                10 => '日',
                _ => 'x',
            });
        }
        cases.push(mixed);

        for text in cases {
            let bytes = text.as_bytes();
            let expected = text.len() + scalar_extra(bytes) + 2;
            assert_eq!(escaped_length(&text), expected, "length {text:?}");
            assert_eq!(
                serde_json::to_string(&text).unwrap().len(),
                expected,
                "serde_json {text:?}"
            );
            if let Some(vector) = escape_extra_vector(bytes) {
                assert_eq!(bytes.len() + vector + 2, expected, "vector {text:?}");
            }
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

    fn nested_array(wraps: usize) -> Value {
        (0..wraps).fold(Value::Null, |value, _| Value::Array(vec![value]))
    }

    #[test]
    fn map_moves_selected_values_and_preserves_fallback_errors() {
        let context = ExecutionContext::new(&|| Ok(()));
        let document = serde_json::json!([
            {"nested": {"payload": ["large", {"value": 1}]}},
            {"nested": {"payload": ["small"]}},
            {"nested": {"payload": null}}
        ]);
        let args = Map::from_iter([
            ("document".into(), document),
            ("select".into(), Value::String("/nested/payload".into())),
            ("default".into(), serde_json::json!({"fallback": true})),
        ]);
        assert_eq!(
            map(&args, &[], &context).expect("selected values are moved to the result"),
            serde_json::json!([[
                "large",
                {"value": 1}
            ], ["small"], {"fallback": true}])
        );

        let args = Map::from_iter([
            ("document".into(), serde_json::json!([{"present": 1}, {}])),
            ("select".into(), Value::String("/present".into())),
        ]);
        assert_eq!(
            map(&args, &[], &context)
                .expect_err("an absent selection still fails")
                .message,
            "select pointer is absent from array item 1"
        );
    }

    #[test]
    fn filter_moves_matching_items_and_keeps_predicate_errors() {
        let context = ExecutionContext::new(&|| Ok(()));
        let args = Map::from_iter([
            (
                "document".into(),
                serde_json::json!([
                    {"keep": true, "nested": [1, {"x": "large"}]},
                    {"keep": false, "nested": [2]},
                    {"keep": true, "nested": [3]}
                ]),
            ),
            (
                "where".into(),
                serde_json::json!([{"pointer": "/keep", "equals": true}]),
            ),
        ]);
        assert_eq!(
            filter(&args, &[], &context).expect("matching items are moved to the result"),
            serde_json::json!([
                {"keep": true, "nested": [1, {"x": "large"}]},
                {"keep": true, "nested": [3]}
            ])
        );

        let args = Map::from_iter([
            ("document".into(), serde_json::json!([[1], [2]])),
            (
                "where".into(),
                serde_json::json!([{"pointer": "/01", "exists": true}]),
            ),
        ]);
        assert!(filter(&args, &[], &context)
            .expect_err("failed predicate evaluation remains an error")
            .message
            .contains("invalid JSON Pointer array index"));
    }

    #[test]
    fn map_output_limit_is_checked_after_moving_defaults() {
        let context = ExecutionContext::new(&|| Ok(()));
        let args = Map::from_iter([
            ("document".into(), serde_json::json!([{}])),
            ("select".into(), Value::String("/missing".into())),
            ("default".into(), Value::String("x".repeat(MAX_JSON_BYTES))),
        ]);
        assert!(map(&args, &[], &context)
            .expect_err("oversized mapped output is rejected")
            .message
            .contains(&format!("{MAX_JSON_BYTES}-byte output limit")));
    }

    #[test]
    fn map_and_filter_take_120000_items_and_match_the_stdin_path() {
        // Issue #262: the array applets are bounded by output bytes, not a fixed item count.
        let context = ExecutionContext::new(&|| Ok(()));
        let items: Vec<Value> = (0..120_000u64)
            .map(|id| serde_json::json!({"id": id, "name": format!("n{id}"), "keep": id % 2 == 0}))
            .collect();
        let document = serde_json::json!({ "items": items });
        let bytes = serde_json::to_vec(&document).expect("document serializes");

        let from_binding = Map::from_iter([
            ("document".into(), document.clone()),
            ("pointer".into(), Value::String("/items".into())),
            ("select".into(), Value::String("/id".into())),
        ]);
        let from_stdin = Map::from_iter([
            ("pointer".into(), Value::String("/items".into())),
            ("select".into(), Value::String("/id".into())),
        ]);
        let mapped = map(&from_binding, &[], &context).expect("120000 items map");
        assert_eq!(mapped.as_array().map(Vec::len), Some(120_000));
        assert_eq!(
            mapped,
            map(&from_stdin, &bytes, &context).expect("stdin map")
        );
        assert_eq!(mapped[119_999], serde_json::json!(119_999));

        let filter_args = Map::from_iter([
            ("document".into(), document),
            ("pointer".into(), Value::String("/items".into())),
            (
                "where".into(),
                serde_json::json!([{"pointer": "/keep", "equals": true}]),
            ),
        ]);
        let kept = filter(&filter_args, &[], &context).expect("120000 items filter");
        assert_eq!(kept.as_array().map(Vec::len), Some(60_000));
        assert_eq!(
            kept[1],
            serde_json::json!({"id": 2, "name": "n2", "keep": true})
        );
    }

    #[test]
    fn entries_output_is_checked_against_depth_and_size_limits() {
        let context = ExecutionContext::new(&|| Ok(()));
        // The object value sits one level deeper in `[ {"key":..,"value":..} ]`
        // than in the input object, so an input one level below the limit
        // produces an output at the limit, and an input at the limit exceeds it.
        for (wraps, accepted) in [(MAX_DEPTH - 2, true), (MAX_DEPTH - 1, false)] {
            let document = serde_json::json!({"k": nested_array(wraps)});
            check_size_and_depth(&document, &context).expect("input is within the limits");
            let args = Map::from_iter([("document".into(), document)]);
            let result = entries(&args, &[], &context);
            if accepted {
                let output = result.expect("entries output at the depth limit is accepted");
                let get_args = Map::from_iter([
                    ("document".into(), output),
                    ("pointer".into(), Value::String("/0/key".into())),
                ]);
                assert_eq!(
                    get(&get_args, &[], &context).expect("shallow get on entries output"),
                    Value::String("k".into())
                );
            } else {
                assert!(result
                    .expect_err("entries output beyond the depth limit is refused")
                    .message
                    .contains(&format!("{MAX_DEPTH}-level depth limit")),);
            }
        }

        // Wrapping one near-limit field adds `[{"key":"k","value":..}]` overhead
        // beyond the input `{"k":..}` encoding, pushing the output over the size limit.
        let document = serde_json::json!({"k": "x".repeat(MAX_JSON_BYTES - 8)});
        assert_eq!(
            check_size_and_depth(&document, &context).expect("input is within the limits"),
            MAX_JSON_BYTES
        );
        let args = Map::from_iter([("document".into(), document)]);
        assert!(entries(&args, &[], &context)
            .expect_err("entries output beyond the size limit is refused")
            .message
            .contains(&format!("{MAX_JSON_BYTES}-byte limit")),);
    }

    #[test]
    fn get_default_is_checked_against_depth_and_size_limits() {
        let context = ExecutionContext::new(&|| Ok(()));
        let typed = crate::structured::parse(br#"{"present":null}"#).expect("typed JSON parses");
        // Both the absent-pointer and null-selection fallbacks return args.default.
        for pointer in ["/missing", "/present"] {
            for wraps in [MAX_DEPTH, MAX_DEPTH + 1] {
                let default = nested_array(wraps);
                let mut args = Map::from_iter([
                    ("pointer".into(), Value::String(pointer.into())),
                    ("default".into(), default.clone()),
                ]);
                let typed_result = get_typed(&args, &typed, &context);
                args.insert("document".into(), serde_json::json!({"present": null}));
                let result = get(&args, &[], &context);
                if wraps == MAX_DEPTH {
                    assert_eq!(
                        result.expect("default at the depth limit is accepted"),
                        default
                    );
                    assert_eq!(
                        typed_result.expect("typed default at the depth limit is accepted"),
                        default
                    );
                } else {
                    for error in [
                        result.expect_err("default beyond the depth limit is refused"),
                        typed_result.expect_err("typed default beyond the depth limit is refused"),
                    ] {
                        assert!(
                            error
                                .message
                                .contains(&format!("{MAX_DEPTH}-level depth limit")),
                            "{}",
                            error.message
                        );
                    }
                }
            }
        }

        let oversized = Value::String("x".repeat(MAX_JSON_BYTES));
        let mut args = Map::from_iter([
            ("pointer".into(), Value::String("/missing".into())),
            ("default".into(), oversized),
        ]);
        assert!(get_typed(&args, &typed, &context)
            .expect_err("typed default beyond the size limit is refused")
            .message
            .contains(&format!("{MAX_JSON_BYTES}-byte limit")),);
        args.insert("document".into(), serde_json::json!({"present": null}));
        assert!(get(&args, &[], &context)
            .expect_err("default beyond the size limit is refused")
            .message
            .contains(&format!("{MAX_JSON_BYTES}-byte limit")),);
    }

    /// `parse` followed by `get_typed`, with errors rendered as the runtime reports them.
    fn parse_then_get(
        bytes: &[u8],
        pointer: &str,
        default: Option<&Value>,
        context: &ExecutionContext<'_>,
    ) -> Result<Value, String> {
        let typed = crate::structured::parse(bytes).map_err(|error| format!("{error:#}"))?;
        let mut args = Map::from_iter([("pointer".to_owned(), Value::String(pointer.to_owned()))]);
        if let Some(default) = default {
            args.insert("default".to_owned(), default.clone());
        }
        get_typed(&args, &typed, context).map_err(|error| error.to_string())
    }

    fn while_parsing(
        bytes: &[u8],
        pointer: &str,
        default: Option<&Value>,
        context: &ExecutionContext<'_>,
    ) -> Result<Value, String> {
        match get_while_parsing(bytes, pointer, default, context) {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(error) => Err(format!("{error:#}")),
        }
    }

    /// Both paths must return the same value, or the same error text.
    fn assert_parity(
        bytes: &[u8],
        pointer: &str,
        default: Option<&Value>,
    ) -> Result<Value, String> {
        let context = ExecutionContext::new(&|| Ok(()));
        let expected = parse_then_get(bytes, pointer, default, &context);
        let actual = while_parsing(bytes, pointer, default, &context);
        let shown = String::from_utf8_lossy(&bytes[..bytes.len().min(80)]).into_owned();
        assert_eq!(actual, expected, "pointer {pointer:?} over {shown:?}");
        expected
    }

    fn nested_bytes(depth: usize) -> Vec<u8> {
        let mut bytes = vec![b'['; depth];
        bytes.push(b'0');
        bytes.extend(std::iter::repeat_n(b']', depth));
        bytes
    }

    #[test]
    fn fused_pointer_resolution_matches_parse_then_get() {
        let document = br#"{"a":{"b":[1,{"c":"x"},null]},"n":null,"s":"text","\u00e9":{"\u65e5":true},"z":{"b":2,"a":1}}"#;
        let fallback = Value::String("fallback".to_owned());
        let pointers = [
            "/a/b/1/c",
            "/a",
            "/a/b/0",
            "/a/b/2",
            "/n",
            "/s/x",
            "/é/日",
            "/z",
            "",
            "/a/b/-",
            "/a/b/5",
            "/a/b/01",
            "/a/b/x",
            "/a/b/99999999999999999999999",
            "/a/b/1/c/deeper",
            "/missing",
            "a",
            "/a~2",
        ];
        for pointer in pointers {
            let _ = assert_parity(document, pointer, None);
            let _ = assert_parity(document, pointer, Some(&fallback));
        }
        assert_eq!(
            assert_parity(document, "/a/b/1/c", None),
            Ok(Value::String("x".to_owned()))
        );
        assert_eq!(assert_parity(document, "/n", None), Ok(Value::Null));
        assert_eq!(
            assert_parity(document, "/n", Some(&fallback)),
            Ok(fallback.clone())
        );
        assert_eq!(
            assert_parity(document, "/missing", Some(&fallback)),
            Ok(fallback.clone())
        );
        assert!(assert_parity(document, "/missing", None)
            .unwrap_err()
            .contains("not_found"));
    }

    #[test]
    fn fused_pointer_unescapes_segments_like_the_applet() {
        let document = br#"{"a/b":{"m~n":{"":5}},"x\"y":1}"#;
        assert_eq!(
            assert_parity(document, "/a~1b/m~0n/", None),
            Ok(serde_json::json!(5))
        );
        assert_eq!(
            assert_parity(document, "/x\"y", None),
            Ok(serde_json::json!(1))
        );
        assert_eq!(
            assert_parity(document, "/a~1b/m~0n", None).map(|value| value.to_string()),
            Ok(r#"{"":5}"#.to_owned())
        );
    }

    #[test]
    fn fused_pointer_reports_duplicate_keys_wherever_they_occur() {
        let documents: [&[u8]; 7] = [
            br#"{"a":1,"a":2}"#,
            br#"{"skip":{"x":1,"x":2},"a":1}"#,
            br#"{"a":1,"skip":[{"y":1,"y":2}]}"#,
            br#"{"a":1,"b":2,"a":3}"#,
            br#"{"a":{"x":1,"x":2}}"#,
            br#"{"a":1,"a":2}"#,
            br#"{"x":{"k":1},"x":{"k":1}}"#,
        ];
        for document in documents {
            for pointer in ["/a", "/x", ""] {
                let result = assert_parity(document, pointer, None);
                assert!(
                    result.clone().unwrap_err().contains("duplicate key"),
                    "{pointer:?} over {document:?}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn fused_pointer_reports_malformed_input_and_tails_like_parse() {
        let documents: [&[u8]; 8] = [
            br#"{"a":1} x"#,
            br#"{"a":1,"b":[1,}"#,
            br#"{"a":1"#,
            br#"{"a":1}}"#,
            br#"{"a":1,"b":tru}"#,
            br#"{"a":1,"b":1e400}"#,
            b"",
            b"   ",
        ];
        for document in documents {
            for pointer in ["/a", ""] {
                assert!(assert_parity(document, pointer, None).is_err());
            }
        }
        assert_eq!(
            assert_parity(br#"{"a":1}   "#, "/a", None),
            Ok(serde_json::json!(1))
        );
        assert_eq!(
            assert_parity(b"\xef\xbb\xbf{\"a\":1}", "/a", None),
            Ok(serde_json::json!(1))
        );
    }

    #[test]
    fn fused_pointer_applies_the_depth_limit_as_parse_does() {
        // serde_json's recursion limit (127 nested containers) decides these before the
        // walk measures depth, and the fused walk reports the same error.
        assert!(assert_parity(&nested_bytes(MAX_DEPTH - 1), "", None).is_ok());
        assert!(assert_parity(&nested_bytes(MAX_DEPTH), "", None)
            .unwrap_err()
            .contains("recursion limit"));
        assert!(assert_parity(&nested_bytes(MAX_DEPTH + 1), "", None)
            .unwrap_err()
            .contains("recursion limit"));
        let mut document = br#"{"a":1,"deep":"#.to_vec();
        document.extend(nested_bytes(MAX_DEPTH + 1));
        document.push(b'}');
        assert!(assert_parity(&document, "/a", None)
            .unwrap_err()
            .contains("recursion limit"));
    }

    #[test]
    fn fused_pointer_applies_the_size_limit_to_the_whole_document() {
        let oversized = format!(r#"{{"a":1,"big":"{}"}}"#, "x".repeat(MAX_JSON_BYTES));
        for pointer in ["/a", "/big", ""] {
            assert!(assert_parity(oversized.as_bytes(), pointer, None)
                .unwrap_err()
                .contains(&format!("{MAX_JSON_BYTES}-byte limit")));
        }

        // The limit is the compact encoding, so a document with more raw bytes than the
        // limit is accepted when its compact form fits.
        let mut padded = br#"{"a":1"#.to_vec();
        padded.extend(std::iter::repeat_n(b' ', MAX_JSON_BYTES));
        padded.push(b'}');
        assert_eq!(assert_parity(&padded, "/a", None), Ok(serde_json::json!(1)));

        // Exactly at the compact limit is accepted; one byte more is refused.
        let base = serde_json::to_vec(&serde_json::json!({"a": 1, "big": ""}))
            .expect("base encodes")
            .len();
        let at_limit = format!(r#"{{"a":1,"big":"{}"}}"#, "x".repeat(MAX_JSON_BYTES - base));
        assert!(assert_parity(at_limit.as_bytes(), "/a", None).is_ok());
        let over_limit = format!(
            r#"{{"a":1,"big":"{}"}}"#,
            "x".repeat(MAX_JSON_BYTES - base + 1)
        );
        assert!(assert_parity(over_limit.as_bytes(), "/a", None).is_err());
    }

    #[test]
    fn fused_pointer_orders_parse_size_pointer_and_lookup_errors_like_get_typed() {
        let context = ExecutionContext::new(&|| Ok(()));
        let oversized = format!(r#"{{"a":1,"big":"{}"}}"#, "x".repeat(MAX_JSON_BYTES));
        let fallback = Value::String("fallback".to_owned());
        // Parse errors come before a malformed pointer.
        assert_eq!(
            parse_then_get(br#"{"a":1"#, "a", None, &context),
            while_parsing(br#"{"a":1"#, "a", None, &context)
        );
        assert!(assert_parity(br#"{"a":1"#, "a", None).is_err());
        // A malformed pointer comes before the lookup but after the document checks.
        assert!(assert_parity(br#"{"a":1}"#, "a", None)
            .unwrap_err()
            .contains("must be empty or start with"));
        assert!(assert_parity(oversized.as_bytes(), "a", None)
            .unwrap_err()
            .contains("byte limit"));
        // The size check runs before the default is consulted.
        assert!(
            assert_parity(oversized.as_bytes(), "/missing", Some(&fallback))
                .unwrap_err()
                .contains("byte limit")
        );
        // A default that cannot be returned is refused only when it would be used.
        let deep_default = nested_array(MAX_DEPTH + 1);
        assert!(
            assert_parity(br#"{"a":1}"#, "/missing", Some(&deep_default))
                .unwrap_err()
                .contains("level depth limit")
        );
        assert_eq!(
            assert_parity(br#"{"a":1}"#, "/a", Some(&deep_default)),
            Ok(serde_json::json!(1))
        );
        let oversized_default = Value::String("x".repeat(MAX_JSON_BYTES));
        assert!(
            assert_parity(br#"{"a":null}"#, "/a", Some(&oversized_default))
                .unwrap_err()
                .contains("byte limit")
        );
        // A lookup error through a list is reported, not replaced by the default.
        assert!(assert_parity(br#"{"x":[1]}"#, "/x/01", Some(&fallback))
            .unwrap_err()
            .contains("invalid JSON Pointer array index"));
    }

    #[test]
    fn fused_pointer_checks_cancellation_after_a_successful_parse_only() {
        let cancelled =
            ExecutionContext::new(&|| Err(crate::applets::ExecutionBudgetExceeded::Cancelled));
        let documents: [&[u8]; 2] = [br#"{"a":1}"#, br#"{"a":1"#];
        for bytes in documents {
            assert_eq!(
                while_parsing(bytes, "/a", None, &cancelled),
                parse_then_get(bytes, "/a", None, &cancelled)
            );
        }
        assert!(while_parsing(br#"{"a":1}"#, "/a", None, &cancelled).is_err());
    }

    #[test]
    fn fused_pointer_selects_numbers_and_key_order_like_the_typed_tree() {
        let document =
            br#"{"n":[18446744073709551615,-0,1.0,-2.5e3,-9223372036854775808],"z":{"b":1,"a":2}}"#;
        for pointer in ["/n", "/z", "", "/n/2", "/n/4"] {
            assert!(assert_parity(document, pointer, None).is_ok());
        }
        assert_eq!(
            assert_parity(document, "/z", None).map(|value| value.to_string()),
            Ok(r#"{"a":2,"b":1}"#.to_owned())
        );
    }

    /// The walk's measured size and depth equal the applet's measurement of the same value.
    #[test]
    fn walked_size_and_depth_equal_the_measured_value() {
        let context = ExecutionContext::new(&|| Ok(()));
        let documents: Vec<Vec<u8>> = vec![
            b"null".to_vec(),
            br#"{"a":[1,-2,3.5,1e300,0.1,18446744073709551615,false,{}]}"#.to_vec(),
            br#"{"quote\"key":"tab\t nl\n cr\r bell\u0007 \\ \u0008\u000c \u00e9 \u65e5\u672c"}"#
                .to_vec(),
            br#"[[[{"x":{"y":[null,"","\u0000\u001f"]}}]]]"#.to_vec(),
            br#"{"meta":{"count":5000},"items":[{"id":1,"tags":["a","b"]}]}"#.to_vec(),
            nested_bytes(MAX_DEPTH - 1),
        ];
        for bytes in documents {
            let value: Value = serde_json::from_slice(&bytes).expect("document parses");
            let measured = check_size_and_depth(&value, &context).expect("value is within limits");
            for segments in [Vec::new(), vec!["a".to_owned()]] {
                let walk = crate::structured::walk_pointer(&bytes, &segments)
                    .expect("walk parses the document");
                assert_eq!(walk.encoded_bytes, measured, "{value}");
                assert_eq!(
                    walk.encoded_bytes,
                    serde_json::to_vec(&value).unwrap().len()
                );
            }
        }
        let walk = crate::structured::walk_pointer(&nested_bytes(MAX_DEPTH - 1), &[])
            .expect("walk parses the document");
        assert_eq!(walk.max_depth, MAX_DEPTH - 1);
    }

    /// Runs `json.map` or `json.filter` over `typed` on both paths. The converted path gets
    /// the document as a serde tree, as the runtime passes it without a typed parameter.
    /// Returns the converted outcome, then the typed one, as the runtime reports them.
    fn applet_both_ways(
        applet: &str,
        args: &Map<String, Value>,
        typed: &TypedValue,
        context: &ExecutionContext<'_>,
    ) -> (Result<Value, String>, Result<Value, String>) {
        let mut converted_args = args.clone();
        converted_args.insert(
            "document".to_owned(),
            crate::structured::to_json(typed).expect("document converts"),
        );
        let (converted, typed_result) = match applet {
            "json.map" => (
                map(&converted_args, &[], context),
                map_typed(args, typed, context),
            ),
            "json.filter" => (
                filter(&converted_args, &[], context),
                filter_typed(args, typed, context),
            ),
            other => panic!("{other} has no typed path"),
        };
        (
            converted.map_err(|error| error.to_string()),
            typed_result.map_err(|error| error.to_string()),
        )
    }

    /// The same document as the runtime may bind it: parsed into the typed tree, and built
    /// from a serde tree.
    fn typed_forms(bytes: &[u8]) -> [TypedValue; 2] {
        let json: Value = serde_json::from_slice(bytes).expect("document is JSON");
        [
            crate::structured::parse(bytes).expect("typed parse succeeds"),
            crate::structured::from_json(json),
        ]
    }

    fn argument_objects(values: Vec<Value>) -> Vec<Map<String, Value>> {
        values
            .into_iter()
            .map(|value| value.as_object().cloned().expect("arguments are an object"))
            .collect()
    }

    fn map_arguments() -> Vec<Map<String, Value>> {
        argument_objects(vec![
            serde_json::json!({"select": "/id"}),
            serde_json::json!({"pointer": "/items", "select": "/id"}),
            serde_json::json!({"pointer": "/items", "select": "/tags/0"}),
            serde_json::json!({"pointer": "/items", "select": "/name", "default": "none"}),
            serde_json::json!({"select": "/v", "default": 0}),
            serde_json::json!({"select": "/missing"}),
            serde_json::json!({"select": "/missing", "default": null}),
            serde_json::json!({"select": ""}),
            serde_json::json!({"select": "/0"}),
            serde_json::json!({"select": "/a~1b"}),
            serde_json::json!({"select": "/ok", "default": {"x": [1, 2.5]}}),
            serde_json::json!({"select": "/n"}),
            serde_json::json!({"pointer": "/items/01", "select": "/id"}),
            serde_json::json!({"pointer": "items", "select": "/id"}),
            serde_json::json!({"pointer": "/missing/x", "select": "/id"}),
            serde_json::json!({"pointer": "/n", "select": "/id"}),
            serde_json::json!({"pointer": "/items/-", "select": "/id"}),
            serde_json::json!({"select": 5}),
            serde_json::json!({}),
        ])
    }

    fn filter_arguments() -> Vec<Map<String, Value>> {
        argument_objects(vec![
            serde_json::json!({"where": []}),
            serde_json::json!({"where": [{"pointer": "/ok", "equals": true}]}),
            serde_json::json!({"where": [{"pointer": "/name", "exists": false}]}),
            serde_json::json!({"where": [{"pointer": "/tags/0", "exists": true}]}),
            serde_json::json!({"where": [{"pointer": "/v", "equals": 1.5}]}),
            serde_json::json!({"where": [{"pointer": "/v", "equals": 1}]}),
            serde_json::json!({"where": [{"pointer": "/id", "not_equals": 1}]}),
            serde_json::json!({"where": [{"pointer": "/k", "type": "number"}]}),
            serde_json::json!({"where": [{"pointer": "/n", "type": "number"}]}),
            serde_json::json!({"where": [{"pointer": "/0", "type": "string"}]}),
            serde_json::json!({"where": [{"equals": {"a": 1}}]}),
            serde_json::json!({"where": [{"equals": null}]}),
            serde_json::json!({"where": [{"pointer": "/tags/x", "exists": true}]}),
            serde_json::json!({"where": [{"pointer": "/tags/01", "exists": true}]}),
            serde_json::json!({"where": [{"pointer": "bad", "exists": true}]}),
            serde_json::json!({"where": "nope"}),
            serde_json::json!({}),
            serde_json::json!({"where": [{"pointer": "/ok"}]}),
            serde_json::json!({"where": [{"exists": true}, {"pointer": "/id", "exists": "yes"}]}),
            serde_json::json!({"where": [{"pointer": "/id", "type": "bogus"}]}),
            serde_json::json!({"pointer": "/items", "where": [{"pointer": "/ok", "equals": true}]}),
            serde_json::json!({"pointer": "/missing", "where": []}),
            serde_json::json!({"pointer": "/items/01", "where": []}),
            serde_json::json!({"pointer": "/n", "where": [{"exists": true}]}),
            serde_json::json!({"where": vec![serde_json::json!({"exists": true}); MAX_FILTER_CLAUSES + 1]}),
        ])
    }

    /// Documents with edge shapes: empty containers, duplicate-looking and unusual keys,
    /// numbers the typed tree and serde represent differently, and escapes.
    const PARITY_DOCUMENTS: [&str; 11] = [
        r#"[]"#,
        r#"{}"#,
        r#"[{}]"#,
        r#"{"items":[]}"#,
        r#"{"items":[{"id":1,"name":"a","tags":["x","y"],"v":1.5,"ok":true},{"id":2,"name":null,"tags":[],"v":-0.0,"ok":false}],"n":5}"#,
        r#"{"a":1,"a ":2,"":3,"A":4,"a/b":5,"a~b":6,"0":7,"-":8,"é":9,"é":10,"ok":{"a":1}}"#,
        r#"[{"0":"zero","1":"one"},["a","b"],"s",7,null,true]"#,
        r#"{"items":[{"n":18446744073709551615},{"n":-9223372036854775808},{"n":9223372036854775807},{"n":1e2},{"n":-0},{"n":-0.0},{"n":1E-7},{"n":0.1},{"n":2.5e300}]}"#,
        r#"{"items":[{"k":"\u0000 nul"},{"k":"tab\tquote\"back\\slash"},{"k":"😀"}]}"#,
        r#"{"items":[{"k":1},{"k":1.0},{"k":"1"},{"k":[1]},{"k":{"1":1}}]}"#,
        r#"{"items":[{"id":1,"tags":["a"]},{"name":"x","tags":[],"ok":false},{"id":3,"tags":{"0":"object"}}],"n":"text"}"#,
    ];

    #[test]
    fn typed_map_and_filter_match_the_converted_path_on_every_shape() {
        let context = ExecutionContext::new(&|| Ok(()));
        let mut successes = 0usize;
        let mut failures = 0usize;
        for bytes in PARITY_DOCUMENTS {
            for typed in typed_forms(bytes.as_bytes()) {
                for (applet, arguments) in [
                    ("json.map", map_arguments()),
                    ("json.filter", filter_arguments()),
                ] {
                    for args in arguments {
                        let (converted, typed_result) =
                            applet_both_ways(applet, &args, &typed, &context);
                        assert_eq!(typed_result, converted, "{applet} {args:?} over {}", bytes);
                        if converted.is_ok() {
                            successes += 1;
                        } else {
                            failures += 1;
                        }
                    }
                }
            }
        }
        assert!(successes > 0 && failures > 0, "both outcomes are covered");
    }

    #[test]
    fn typed_map_and_filter_match_the_converted_path_on_a_120000_item_array() {
        let context = ExecutionContext::new(&|| Ok(()));
        let items: Vec<Value> = (0..120_000u64)
            .map(|index| {
                serde_json::json!({
                    "id": index,
                    "name": format!("n{index}"),
                    "tags": ["a", "b"],
                    "v": index as f64 * 1.5,
                })
            })
            .collect();
        let typed = crate::structured::from_json(serde_json::json!({"items": items}));
        // Each case names the applet, its arguments and the length of its result.
        let cases = [
            (
                "json.map",
                serde_json::json!({"pointer": "/items", "select": "/id"}),
                120_000,
            ),
            (
                "json.map",
                serde_json::json!({"pointer": "/items", "select": "/tags/1"}),
                120_000,
            ),
            (
                "json.map",
                serde_json::json!({"pointer": "/items", "select": "/missing", "default": 0}),
                120_000,
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/items", "where": [{"pointer": "/id", "equals": 119_999}]}),
                1,
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/items", "where": [
                    {"pointer": "/name", "exists": true},
                    {"pointer": "/v", "type": "number"}
                ]}),
                120_000,
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/items", "where": [{"pointer": "/tags/0", "equals": "a"}]}),
                120_000,
            ),
        ];
        for (applet, value, length) in cases {
            let args = value.as_object().cloned().expect("arguments are an object");
            let (converted, typed_result) = applet_both_ways(applet, &args, &typed, &context);
            assert_eq!(typed_result, converted, "{applet} {args:?}");
            let result = converted.expect("the 120000-item case succeeds");
            assert_eq!(
                result.as_array().map(Vec::len),
                Some(length),
                "{applet} {args:?}"
            );
        }
    }

    #[test]
    fn typed_map_and_filter_report_the_converted_paths_errors() {
        let context = ExecutionContext::new(&|| Ok(()));
        let typed =
            crate::structured::parse(br#"{"items":[{"id":1,"tags":["a"]},{"name":"x"}],"n":5}"#)
                .expect("typed JSON parses");
        let cases = [
            (
                "json.map",
                serde_json::json!({"pointer": "/items", "select": "/id"}),
                "select pointer is absent from array item 1",
            ),
            (
                "json.map",
                serde_json::json!({"pointer": "/n", "select": "/id"}),
                "the selected JSON value must be an array",
            ),
            (
                "json.map",
                serde_json::json!({"pointer": "/missing", "select": "/id"}),
                "the selected JSON value must be an array",
            ),
            (
                "json.map",
                serde_json::json!({"pointer": "/items/01", "select": "/id"}),
                "invalid JSON Pointer array index",
            ),
            (
                "json.map",
                serde_json::json!({"pointer": "items", "select": "/id"}),
                "JSON Pointer must be empty or start with '/'",
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/items", "where": [{"pointer": "/id", "type": "bogus"}]}),
                "must be null, boolean, number, string, array, or object",
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/items", "where": [{"pointer": "/tags/01", "exists": true}]}),
                "invalid JSON Pointer array index",
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/n"}),
                "the selected JSON value must be an array",
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/items"}),
                "args.where must be an array of predicate objects",
            ),
            (
                "json.filter",
                serde_json::json!({"pointer": "/items", "where": [{"exists": true}, {"pointer": "/id", "exists": "yes"}]}),
                "args.where[1].exists must be a boolean",
            ),
        ];
        for (applet, value, expected) in cases {
            let args = value.as_object().cloned().expect("arguments are an object");
            let (converted, typed_result) = applet_both_ways(applet, &args, &typed, &context);
            assert_eq!(typed_result, converted, "{applet} {args:?}");
            let error = converted.expect_err("the case is refused");
            assert!(
                error.contains(expected),
                "{error} does not contain {expected}"
            );
        }

        // A document that is both too deep and given a malformed pointer reports the depth
        // first, on both paths, as the converted path checks the document before the pointer.
        let too_deep =
            (0..=MAX_DEPTH).fold(TypedValue::Null, |value, _| TypedValue::List(vec![value]));
        let args = Map::from_iter([("pointer".to_owned(), Value::String("x".to_owned()))]);
        let (converted, typed_result) = applet_both_ways("json.map", &args, &too_deep, &context);
        assert_eq!(typed_result, converted);
        assert!(converted
            .expect_err("deep JSON is refused")
            .contains("depth limit"));
        let (converted, typed_result) = applet_both_ways("json.filter", &args, &too_deep, &context);
        assert_eq!(typed_result, converted);
        assert!(converted
            .expect_err("deep JSON is refused")
            .contains("depth limit"));
    }

    /// The typed tree and serde agree on every edge number that JSON text can hold, so
    /// the converted path and the typed path see the same values.
    #[test]
    fn typed_numbers_at_the_edges_match_the_converted_path() {
        let context = ExecutionContext::new(&|| Ok(()));
        let literals = [
            "18446744073709551615",
            "-9223372036854775808",
            "9223372036854775807",
            "1e2",
            "-0",
            "-0.0",
            "1E-7",
            "0.1",
            "2.5e300",
            "1.0",
            "100",
        ];
        for literal in literals {
            let bytes = format!(r#"{{"items":[{{"n":{literal}}},{{"n":100}}]}}"#);
            let json: Value = serde_json::from_str(&bytes).expect("literal is JSON");
            for typed in typed_forms(bytes.as_bytes()) {
                assert_eq!(
                    crate::structured::to_json(&typed).expect("document converts"),
                    json,
                    "typed tree for {literal}"
                );
                let number = json["items"][0]["n"].clone();
                for (applet, value) in [
                    (
                        "json.filter",
                        serde_json::json!({"pointer": "/items", "where": [{"pointer": "/n", "equals": number}]}),
                    ),
                    (
                        "json.filter",
                        serde_json::json!({"pointer": "/items", "where": [{"pointer": "/n", "not_equals": 100}]}),
                    ),
                    (
                        "json.filter",
                        serde_json::json!({"pointer": "/items", "where": [{"pointer": "/n", "type": "number"}]}),
                    ),
                    (
                        "json.map",
                        serde_json::json!({"pointer": "/items", "select": "/n"}),
                    ),
                ] {
                    let args = value.as_object().cloned().expect("arguments are an object");
                    let (converted, typed_result) =
                        applet_both_ways(applet, &args, &typed, &context);
                    assert_eq!(typed_result, converted, "{applet} {args:?} for {literal}");
                    assert!(converted.is_ok(), "{literal}");
                }
            }
        }
    }
}
