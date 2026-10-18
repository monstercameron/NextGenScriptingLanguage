//! Small typed list and line aggregation operations.
use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

const MAX_ITEMS: usize = 1_000_000;
const MAX_BYTES: usize = 16 * 1024 * 1024;

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_BYTES {
            return Err(io::Error::other("input exceeds byte budget"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fits_byte_budget(value: &(impl serde::Serialize + ?Sized)) -> bool {
    serde_json::to_writer(ByteBudget(0), value).is_ok()
}

pub(super) fn check_values(values: &[Value]) -> Result<(), AppletError> {
    if values.len() > MAX_ITEMS || !fits_byte_budget(values) {
        return Err(invalid(format!(
            "list exceeds the bounded {MAX_ITEMS}-item or {MAX_BYTES}-byte limit"
        )));
    }
    Ok(())
}

pub(super) fn check_bytes(length: usize) -> Result<(), AppletError> {
    if length > MAX_BYTES {
        return Err(invalid(format!(
            "input exceeds the bounded {MAX_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn items(args: &Map<String, Value>, input: &[u8]) -> Result<Vec<Value>, AppletError> {
    if let Some(value) = args.get("items") {
        let array = value
            .as_array()
            .ok_or_else(|| invalid("args.items must be a list"))?;
        check_values(array)?;
        return Ok(array.clone());
    }
    let text = std::str::from_utf8(input)
        .map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))?;
    check_bytes(input.len())?;
    if text.lines().take(MAX_ITEMS + 1).count() > MAX_ITEMS {
        return Err(invalid(format!(
            "stdin exceeds the bounded {MAX_ITEMS}-line or {MAX_BYTES}-byte limit"
        )));
    }
    Ok(text
        .lines()
        .map(|line| Value::String(line.to_owned()))
        .collect())
}

pub(super) fn run(
    name: &str,
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    if name == "list.count" && !args.contains_key("items") {
        let text = std::str::from_utf8(input)
            .map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))?;
        check_bytes(input.len())?;
        let mut count = 0;
        for (index, _) in text.lines().enumerate() {
            checkpoint_work(context, index)?;
            count += 1;
        }
        if count > MAX_ITEMS {
            return Err(invalid(format!(
                "stdin exceeds the bounded {MAX_ITEMS}-line limit"
            )));
        }
        return Ok(Value::from(count));
    }
    let mut values = items(args, input)?;
    match name {
        "list.count" => Ok(Value::from(values.len())),
        "list.sum" => {
            let mut integer_total: Option<i128> = Some(0);
            let mut float_total = 0.0f64;
            for (index, value) in values.iter().enumerate() {
                checkpoint_work(context, index)?;
                let number = value
                    .as_number()
                    .ok_or_else(|| invalid(format!("items[{index}] must be numeric")))?;
                if let Some(integer) = number
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| number.as_u64().map(i128::from))
                {
                    if let Some(total) = integer_total.as_mut() {
                        *total = total
                            .checked_add(integer)
                            .ok_or_else(|| super::failed("overflow", "integer sum overflowed"))?;
                    } else {
                        float_total += integer as f64;
                    }
                } else {
                    if let Some(total) = integer_total.take() {
                        float_total += total as f64;
                    }
                    float_total += number
                        .as_f64()
                        .ok_or_else(|| invalid(format!("items[{index}] must be finite numeric")))?;
                }
                if !float_total.is_finite() {
                    return Err(invalid("list.sum result is not finite"));
                }
            }
            if let Some(total) = integer_total {
                return Number::from_i128(total)
                    .map(Value::Number)
                    .ok_or_else(|| super::failed("overflow", "integer sum exceeds JSON range"));
            }
            Number::from_f64(float_total)
                .map(Value::Number)
                .ok_or_else(|| invalid("list.sum result is not finite"))
        }
        "list.min" | "list.max" => {
            let mut best: Option<Value> = None;
            for (index, value) in values.into_iter().enumerate() {
                checkpoint_work(context, index)?;
                if value.as_number().is_none() {
                    return Err(invalid(format!("items[{index}] must be numeric")));
                }
                if best.as_ref().is_none_or(|current| {
                    let ordering = match (value.as_number(), current.as_number()) {
                        (Some(value), Some(current)) => numeric_compare(value, current),
                        _ => Ordering::Equal,
                    };
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
            for (index, value) in values.into_iter().enumerate() {
                checkpoint_work(context, index)?;
                let key = equality_key(&value)?;
                if seen.insert(key) {
                    unique.push(value);
                }
            }
            Ok(Value::Array(unique))
        }
        "list.sort" | "list.top" => {
            let pointer = args.get("key").and_then(Value::as_str).unwrap_or("");
            let comparisons = std::cell::Cell::new(0usize);
            let checkpoint_error = std::cell::RefCell::new(None);
            values.sort_by(|left, right| {
                if checkpoint_error.borrow().is_some() {
                    return Ordering::Equal;
                }
                let next = comparisons.get() + 1;
                comparisons.set(next);
                if let Err(error) = checkpoint_work(context, next) {
                    *checkpoint_error.borrow_mut() = Some(error);
                    return Ordering::Equal;
                }
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
            if let Some(error) = checkpoint_error.into_inner() {
                return Err(error);
            }
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
        "list.group" => group(values, args, context),
        "text.count_lines_by" => count_lines_by(input, args, context),
        _ => Err(invalid(format!("unknown aggregation {name:?}"))),
    }
}

fn compare(left: Option<&Value>, right: Option<&Value>) -> Ordering {
    match (left, right) {
        (Some(Value::Number(a)), Some(Value::Number(b))) => numeric_compare(a, b),
        (Some(Value::String(a)), Some(Value::String(b))) => a.cmp(b),
        (Some(a), Some(b)) => a.to_string().cmp(&b.to_string()),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

fn group(
    values: Vec<Value>,
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
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
    for (index, item) in values.into_iter().enumerate() {
        checkpoint_work(context, index)?;
        let key = item
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| invalid(format!("group key {pointer:?} is absent")))?;
        let encoded_key = equality_key(&key)?;
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

pub(super) fn numeric_order(a: &Number, b: &Number) -> Result<Ordering, AppletError> {
    let ai = a
        .as_i64()
        .map(i128::from)
        .or_else(|| a.as_u64().map(i128::from));
    let bi = b
        .as_i64()
        .map(i128::from)
        .or_else(|| b.as_u64().map(i128::from));
    match (ai, bi) {
        (Some(a), Some(b)) => Ok(a.cmp(&b)),
        (Some(integer), None) => compare_integer_float(
            integer,
            b.as_f64()
                .ok_or_else(|| invalid("numeric value must be finite"))?,
        ),
        (None, Some(integer)) => compare_integer_float(
            integer,
            a.as_f64()
                .ok_or_else(|| invalid("numeric value must be finite"))?,
        )
        .map(Ordering::reverse),
        (None, None) => a
            .as_f64()
            .zip(b.as_f64())
            .and_then(|(a, b)| a.partial_cmp(&b))
            .ok_or_else(|| invalid("numeric values must be finite")),
    }
}

fn numeric_compare(a: &Number, b: &Number) -> Ordering {
    numeric_order(a, b).unwrap_or(Ordering::Equal)
}

fn compare_integer_float(integer: i128, float: f64) -> Result<Ordering, AppletError> {
    if !float.is_finite() {
        return Err(invalid("numeric value must be finite"));
    }
    if float >= 18_446_744_073_709_551_616.0 {
        return Ok(Ordering::Less);
    }
    if float < i64::MIN as f64 {
        return Ok(Ordering::Greater);
    }
    let truncated = float.trunc() as i128;
    Ok(match integer.cmp(&truncated) {
        Ordering::Equal if float > truncated as f64 => Ordering::Less,
        Ordering::Equal if float < truncated as f64 => Ordering::Greater,
        ordering => ordering,
    })
}

fn equality_key(value: &Value) -> Result<String, AppletError> {
    if let Some(number) = value.as_number() {
        if let Some(integer) = number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
        {
            return Ok(format!("n:{integer}"));
        }
        if let Some(float) = number.as_f64() {
            if float.fract() == 0.0 && float >= i64::MIN as f64 && float <= u64::MAX as f64 {
                return Ok(format!("n:{float:.0}"));
            }
            return Ok(format!(
                "f:{}",
                if float == 0.0 {
                    "0".to_owned()
                } else {
                    float.to_string()
                }
            ));
        }
    }
    serde_json::to_string(value)
        .map_err(|error| invalid(format!("cannot compare list value: {error}")))
}

fn count_lines_by(
    input: &[u8],
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = std::str::from_utf8(input)
        .map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))?;
    let delimiter = args
        .get("delimiter")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("text.count_lines_by requires args.delimiter"))?;
    if delimiter.is_empty() {
        return Err(invalid("args.delimiter cannot be empty"));
    }
    check_bytes(input.len())?;
    let field = usize::try_from(args.get("field").and_then(Value::as_u64).unwrap_or(0))
        .map_err(|_| invalid("args.field is too large"))?;
    // Keys keep first-seen order; the index makes each line a constant-time lookup.
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (number, line) in text.lines().enumerate() {
        checkpoint_work(context, number)?;
        if number >= MAX_ITEMS {
            return Err(invalid(format!(
                "stdin exceeds the bounded {MAX_ITEMS}-line limit"
            )));
        }
        let key = if delimiter.len() == 1 {
            let mut remaining = line;
            for _ in 0..field {
                let Some(position) = memchr::memchr(delimiter.as_bytes()[0], remaining.as_bytes())
                else {
                    return Err(invalid(format!("line has no field {field}")));
                };
                remaining = &remaining[position + 1..];
            }
            remaining
                .split_once(delimiter)
                .map_or(remaining, |(key, _)| key)
        } else {
            line.split(delimiter)
                .nth(field)
                .ok_or_else(|| invalid(format!("line has no field {field}")))?
        };
        match index.get(key) {
            Some(&position) => counts[position].1 += 1,
            None => {
                index.insert(key, counts.len());
                counts.push((key, 1));
            }
        }
    }
    Ok(Value::Array(
        counts
            .into_iter()
            .map(|(key, count)| serde_json::json!({"key":key,"count":count}))
            .collect(),
    ))
}
