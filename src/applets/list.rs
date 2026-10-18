use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Map, Value};

const LIMIT: usize = 100_000;

fn list<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a [Value], AppletError> {
    let values = args
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("args.{name} must be a list")))?;
    if values.len() > LIMIT {
        return Err(invalid(format!("list exceeds {LIMIT} items")));
    }
    super::aggregation::check_values(values)?;
    Ok(values)
}

fn index(value: i64, len: usize) -> Result<usize, AppletError> {
    let index = if value < 0 {
        (len as i64).checked_add(value)
    } else {
        Some(value)
    }
    .ok_or_else(|| invalid("list index is out of range"))?;
    usize::try_from(index)
        .ok()
        .filter(|index| *index <= len)
        .ok_or_else(|| invalid("list index is out of range"))
}

pub(super) fn run(
    operation: &str,
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    match operation {
        "list.range" => {
            let start = args
                .get("start")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("args.start must be an integer"))?;
            let stop = args
                .get("stop")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("args.stop must be an integer"))?;
            let step = match args.get("step") {
                None => {
                    if start <= stop {
                        1
                    } else {
                        -1
                    }
                }
                Some(value) => value
                    .as_i64()
                    .ok_or_else(|| invalid("args.step must be an integer"))?,
            };
            if step == 0 {
                return Err(invalid("args.step cannot be zero"));
            }
            let mut values = Vec::new();
            let mut current = start;
            while if step > 0 {
                current < stop
            } else {
                current > stop
            } {
                if values.len() == LIMIT {
                    return Err(invalid("list.range exceeds the 100000 item limit"));
                }
                checkpoint_work(context, values.len())?;
                values.push(Value::from(current));
                current = current
                    .checked_add(step)
                    .ok_or_else(|| invalid("list.range integer overflow"))?;
            }
            Ok(Value::Array(values))
        }
        "list.enumerate" => {
            let mut output = Vec::new();
            for (index, item) in list(args, "items")?.iter().enumerate() {
                checkpoint_work(context, index)?;
                output.push(serde_json::json!({"index":index,"item":item}));
            }
            Ok(Value::Array(output))
        }
        "list.slice" => {
            let values = list(args, "items")?;
            let offset = args
                .get("offset")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("args.offset must be an integer"))?;
            let start = index(offset, values.len())?;
            let length = args
                .get("length")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("args.length must be nonnegative"))?
                as usize;
            let end = start
                .checked_add(length)
                .filter(|end| *end <= values.len())
                .ok_or_else(|| invalid("list.slice range is out of bounds"))?;
            Ok(Value::Array(values[start..end].to_vec()))
        }
        "list.first" | "list.last" | "list.rest" => {
            let values = list(args, "items")?;
            match operation {
                "list.first" => values
                    .first()
                    .cloned()
                    .ok_or_else(|| invalid("list.first requires a nonempty list")),
                "list.last" => values
                    .last()
                    .cloned()
                    .ok_or_else(|| invalid("list.last requires a nonempty list")),
                _ => Ok(Value::Array(values.get(1..).unwrap_or_default().to_vec())),
            }
        }
        "list.reverse" => {
            let mut result = list(args, "items")?.to_vec();
            result.reverse();
            Ok(Value::Array(result))
        }
        "list.concat" => {
            let lists = list(args, "items")?;
            let mut out = Vec::new();
            for (idx, sublist) in lists.iter().enumerate() {
                checkpoint_work(context, idx)?;
                let sublist = sublist
                    .as_array()
                    .ok_or_else(|| invalid(format!("args.items[{idx}] must be a list")))?;
                if out
                    .len()
                    .checked_add(sublist.len())
                    .is_none_or(|size| size > LIMIT)
                {
                    return Err(invalid("list.concat exceeds the 100000 item limit"));
                }
                out.extend(sublist.iter().cloned());
            }
            Ok(Value::Array(out))
        }
        "list.flatten" => {
            let depth = match args.get("depth") {
                None => u64::MAX,
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| invalid("args.depth must be a nonnegative integer"))?,
            };
            let mut out = Vec::new();
            let mut pending = list(args, "items")?
                .iter()
                .rev()
                .map(|value| (value, depth))
                .collect::<Vec<_>>();
            let mut work = 0usize;
            while let Some((value, remaining)) = pending.pop() {
                checkpoint_work(context, work)?;
                work += 1;
                if work > 1_000_000 {
                    return Err(invalid("list.flatten exceeds the traversal limit"));
                }
                if let Some(values) = value.as_array().filter(|_| remaining > 0) {
                    if pending
                        .len()
                        .checked_add(values.len())
                        .is_none_or(|size| size > 1_000_000)
                    {
                        return Err(invalid("list.flatten exceeds the traversal limit"));
                    }
                    pending.extend(values.iter().rev().map(|item| (item, remaining - 1)));
                } else {
                    if out.len() == LIMIT {
                        return Err(invalid("list.flatten exceeds the 100000 item limit"));
                    }
                    out.push(value.clone());
                }
            }
            Ok(Value::Array(out))
        }
        "list.zip" => {
            let left = list(args, "left")?;
            let right = list(args, "right")?;
            let mut output = Vec::with_capacity(left.len().min(right.len()));
            for (index, (a, b)) in left.iter().zip(right).enumerate() {
                checkpoint_work(context, index)?;
                output.push(Value::Array(vec![a.clone(), b.clone()]));
            }
            Ok(Value::Array(output))
        }
        "list.chunk" => {
            let values = list(args, "items")?;
            let size = args
                .get("size")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid("args.size must be a positive integer"))?;
            let mut output = Vec::with_capacity(values.len().div_ceil(size));
            for (index, chunk) in values.chunks(size).enumerate() {
                checkpoint_work(context, index)?;
                output.push(Value::Array(chunk.to_vec()));
            }
            Ok(Value::Array(output))
        }
        _ => Err(invalid(format!("unknown list operation {operation:?}"))),
    }
}
