//! Small typed list and line aggregation operations.
use super::{invalid, AppletError};
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

const MAX_ITEMS: usize = 1_000_000;
const MAX_BYTES: usize = 16 * 1024 * 1024;

fn items(args: &Map<String, Value>, input: &[u8]) -> Result<Vec<Value>, AppletError> {
    if let Some(value) = args.get("items") {
        return value
            .as_array()
            .cloned()
            .ok_or_else(|| invalid("args.items must be a list"));
    }
    let text = std::str::from_utf8(input)
        .map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))?;
    Ok(text
        .lines()
        .map(|line| Value::String(line.to_owned()))
        .collect())
}

fn bounded(values: &[Value]) -> Result<(), AppletError> {
    if values.len() > MAX_ITEMS
        || serde_json::to_vec(values).map_or(true, |bytes| bytes.len() > MAX_BYTES)
    {
        return Err(invalid(format!(
            "list exceeds the bounded {MAX_ITEMS}-item or {MAX_BYTES}-byte limit"
        )));
    }
    Ok(())
}

pub(super) fn run(
    name: &str,
    args: &Map<String, Value>,
    input: &[u8],
) -> Result<Value, AppletError> {
    if name == "list.count" && !args.contains_key("items") {
        let text = std::str::from_utf8(input)
            .map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))?;
        return Ok(Value::from(text.lines().count()));
    }
    let mut values = items(args, input)?;
    bounded(&values)?;
    match name {
        "list.count" => Ok(Value::from(values.len())),
        "list.sum" => {
            let mut total = 0.0f64;
            for (index, value) in values.iter().enumerate() {
                let number = value
                    .as_f64()
                    .ok_or_else(|| invalid(format!("items[{index}] must be numeric")))?;
                total += number;
                if !total.is_finite() {
                    return Err(invalid("list.sum result is not finite"));
                }
            }
            Number::from_f64(total)
                .map(Value::Number)
                .ok_or_else(|| invalid("list.sum result is not finite"))
        }
        "list.min" | "list.max" => {
            let mut best: Option<Value> = None;
            for (index, value) in values.into_iter().enumerate() {
                if value.as_f64().is_none() {
                    return Err(invalid(format!("items[{index}] must be numeric")));
                }
                if best.as_ref().is_none_or(|current| {
                    let ordering = value
                        .as_f64()
                        .unwrap_or_default()
                        .total_cmp(&current.as_f64().unwrap_or_default());
                    (name == "list.min" && ordering == Ordering::Less)
                        || (name == "list.max" && ordering == Ordering::Greater)
                }) {
                    best = Some(value);
                }
            }
            best.ok_or_else(|| invalid("list.min/list.max requires at least one item"))
        }
        "list.unique" => {
            let mut unique = Vec::new();
            let mut seen = HashSet::new();
            for value in values {
                let key = serde_json::to_string(&value)
                    .map_err(|error| invalid(format!("cannot compare list value: {error}")))?;
                if seen.insert(key) {
                    unique.push(value);
                }
            }
            Ok(Value::Array(unique))
        }
        "list.sort" | "list.top" => {
            let pointer = args.get("key").and_then(Value::as_str).unwrap_or("");
            values.sort_by(|left, right| {
                let l = if pointer.is_empty() {
                    Some(left)
                } else {
                    left.pointer(pointer)
                };
                let r = if pointer.is_empty() {
                    Some(right)
                } else {
                    right.pointer(pointer)
                };
                compare(l, r)
            });
            if name == "list.top" {
                let n = args
                    .get("n")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("args.n must be a nonnegative integer"))?
                    .try_into()
                    .map_err(|_| invalid("args.n exceeds the platform list size"))?;
                values.reverse();
                values.truncate(n);
            }
            Ok(Value::Array(values))
        }
        "list.group" => group(values, args),
        "text.count_lines_by" => count_lines_by(input, args),
        _ => Err(invalid(format!("unknown aggregation {name:?}"))),
    }
}

fn compare(left: Option<&Value>, right: Option<&Value>) -> Ordering {
    match (left, right) {
        (Some(Value::Number(a)), Some(Value::Number(b))) => a
            .as_f64()
            .unwrap_or_default()
            .total_cmp(&b.as_f64().unwrap_or_default()),
        (Some(Value::String(a)), Some(Value::String(b))) => a.cmp(b),
        (Some(a), Some(b)) => a.to_string().cmp(&b.to_string()),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

fn group(values: Vec<Value>, args: &Map<String, Value>) -> Result<Value, AppletError> {
    let pointer = args
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("list.group requires a JSON Pointer in args.key"))?;
    let collect = args
        .get("collect")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut groups: Vec<(Value, usize, Vec<Value>)> = Vec::new();
    let mut indexes: HashMap<String, usize> = HashMap::new();
    for item in values {
        let key = item
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| invalid(format!("group key {pointer:?} is absent")))?;
        let encoded_key = serde_json::to_string(&key)
            .map_err(|error| invalid(format!("cannot compare group key: {error}")))?;
        if let Some(index) = indexes.get(&encoded_key).copied() {
            let (_, count, items) = &mut groups[index];
            *count += 1;
            if collect {
                items.push(item);
            }
        } else {
            indexes.insert(encoded_key, groups.len());
            groups.push((key, 1, if collect { vec![item] } else { Vec::new() }));
        }
    }
    Ok(Value::Array(
        groups
            .into_iter()
            .map(|(key, count, items)| {
                let mut row = Map::from_iter([
                    ("key".to_owned(), key),
                    ("count".to_owned(), Value::from(count)),
                ]);
                if collect {
                    row.insert("items".to_owned(), Value::Array(items));
                }
                Value::Object(row)
            })
            .collect(),
    ))
}

fn count_lines_by(input: &[u8], args: &Map<String, Value>) -> Result<Value, AppletError> {
    let text = std::str::from_utf8(input)
        .map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))?;
    let delimiter = args
        .get("delimiter")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("text.count_lines_by requires args.delimiter"))?;
    if delimiter.is_empty() {
        return Err(invalid("args.delimiter cannot be empty"));
    }
    let field = args.get("field").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut counts: Vec<(String, usize)> = Vec::new();
    for line in text.lines() {
        let key = line
            .split(delimiter)
            .nth(field)
            .ok_or_else(|| invalid(format!("line has no field {field}")))?;
        if let Some((_, count)) = counts.iter_mut().find(|(candidate, _)| candidate == key) {
            *count += 1;
        } else {
            counts.push((key.to_owned(), 1));
        }
    }
    Ok(Value::Array(
        counts
            .into_iter()
            .map(|(key, count)| serde_json::json!({"key":key,"count":count}))
            .collect(),
    ))
}
