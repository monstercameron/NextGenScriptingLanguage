//! Bounded, declarative transforms for JSON values.
//!
//! The predicates and paths accepted here are data, never executable source.
//! JSON Pointer uses RFC 6901 escaping and indexes arrays by decimal position.

use super::{absence, checkpoint_work, invalid, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};

const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_POINTER_BYTES: usize = 4096;
const MAX_POINTER_SEGMENTS: usize = 256;
const MAX_FILTER_CLAUSES: usize = 32;
const MAX_COLLECTION_ITEMS: usize = 100_000;
const MAX_DEPTH: usize = 128;

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
    let value = serde_json::from_slice(text)
        .map_err(|error| invalid(format!("text is not valid JSON: {error}")))?;
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
    check_size_and_depth(value, context)?;
    let text = serde_json::to_string(value)
        .map_err(|error| invalid(format!("JSON value cannot be serialized: {error}")))?;
    Ok(Value::String(text))
}

pub(super) fn get(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let document = document(args, input, context)?;
    let pointer = parse_pointer(optional_pointer(args, "pointer")?)?;
    lookup(&document, &pointer)?
        .cloned()
        .ok_or_else(|| absence("not_found", "JSON Pointer does not identify a value"))
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
    if array.len() > MAX_COLLECTION_ITEMS {
        return Err(invalid(format!(
            "selected array exceeds the {MAX_COLLECTION_ITEMS}-item limit"
        )));
    }
    let mut mapped = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        checkpoint_work(context, index)?;
        let value = lookup(item, &selection_pointer)?.ok_or_else(|| {
            absence(
                "not_found",
                format!("select pointer is absent from array item {index}"),
            )
        })?;
        mapped.push(value.clone());
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
    if array.len() > MAX_COLLECTION_ITEMS {
        return Err(invalid(format!(
            "selected array exceeds the {MAX_COLLECTION_ITEMS}-item limit"
        )));
    }
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
            selected.push(item.clone());
        }
    }
    let result = Value::Array(selected);
    check_size_and_depth(&result, context)?;
    Ok(result)
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
        serde_json::from_slice(input)
            .map_err(|error| invalid(format!("stdin is not valid JSON: {error}")))?
    };
    check_size_and_depth(&value, context)?;
    Ok(value)
}

fn check_size_and_depth(value: &Value, context: &ExecutionContext<'_>) -> Result<(), AppletError> {
    let mut pending = vec![(value, 0usize)];
    let mut work = 0usize;
    while let Some((value, depth)) = pending.pop() {
        checkpoint_work(context, work)?;
        work += 1;
        if depth > MAX_DEPTH {
            return Err(invalid(format!(
                "JSON value exceeds the {MAX_DEPTH}-level depth limit"
            )));
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    checkpoint_work(context, work)?;
                    work += 1;
                    pending.push((item, depth + 1));
                }
            }
            Value::Object(fields) => {
                for item in fields.values() {
                    checkpoint_work(context, work)?;
                    work += 1;
                    pending.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }
    context.checkpoint()?;
    let encoded = serde_json::to_vec(value)
        .map_err(|error| invalid(format!("JSON value cannot be serialized: {error}")))?;
    context.checkpoint()?;
    if encoded.len() > MAX_JSON_BYTES {
        return Err(invalid(format!(
            "JSON value exceeds the {MAX_JSON_BYTES}-byte limit"
        )));
    }
    Ok(())
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
            Value::Array(items) => match parse_array_index(segment)? {
                Some(index) => match items.get(index) {
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
            Self::Equals(pointer, expected) => lookup(item, pointer)? == Some(expected),
            // A missing value is not equal to any JSON value. This also means
            // an absent nested field naturally passes a `not_equals` filter.
            Self::NotEquals(pointer, expected) => lookup(item, pointer)? != Some(expected),
            Self::Type(pointer, expected) => {
                lookup(item, pointer)?.is_some_and(|value| json_type(value) == expected)
            }
        })
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
