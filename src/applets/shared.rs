//! Shared bounds, numeric ordering, and JSON equality for applet operations.
use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Number, Value};
use std::cmp::Ordering;

pub(super) const MAX_ITEMS: usize = 1_000_000;
pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_DEPTH: usize = 128;

pub(super) fn check_values(
    values: &[Value],
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    if values.len() > MAX_ITEMS {
        return Err(invalid(format!(
            "list exceeds the bounded {MAX_ITEMS}-item or {MAX_BYTES}-byte limit"
        )));
    }
    let pending = values.iter().map(|value| (value, 1usize)).collect();
    measure_json(pending, 2 + values.len().saturating_sub(1), context)?;
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

// A canonical, type-tagged, length-framed key. Equal numbers use the same
// representation at any depth; integer conversion is bounded before casting.
pub(super) fn equality_key(
    value: &Value,
    context: &ExecutionContext<'_>,
) -> Result<Vec<u8>, AppletError> {
    enum Part<'a> {
        Value(&'a Value),
        String(&'a str),
    }
    fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), AppletError> {
        if out.len().saturating_add(bytes.len()) > MAX_BYTES.saturating_mul(16) {
            return Err(invalid("JSON equality key exceeds the bounded limit"));
        }
        out.extend_from_slice(bytes);
        Ok(())
    }
    let mut out = Vec::new();
    let mut stack = vec![Part::Value(value)];
    let mut work = 0usize;
    while let Some(part) = stack.pop() {
        checkpoint_work(context, work)?;
        work += 1;
        match part {
            Part::String(s) => {
                append(&mut out, &(s.len() as u64).to_be_bytes())?;
                append(&mut out, s.as_bytes())?;
            }
            Part::Value(Value::Null) => append(&mut out, b"N")?,
            Part::Value(Value::Bool(false)) => append(&mut out, b"F")?,
            Part::Value(Value::Bool(true)) => append(&mut out, b"T")?,
            Part::Value(Value::String(s)) => {
                append(&mut out, b"S")?;
                stack.push(Part::String(s));
            }
            Part::Value(Value::Number(n)) => {
                let integer = n
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| n.as_u64().map(i128::from))
                    .or_else(|| {
                        n.as_f64().and_then(|f| {
                            (f.fract() == 0.0
                                && f >= i64::MIN as f64
                                && f < 18_446_744_073_709_551_616.0)
                                .then_some(f as i128)
                        })
                    });
                if let Some(i) = integer {
                    append(&mut out, b"I")?;
                    append(&mut out, &i.to_be_bytes())?;
                } else {
                    let f = n
                        .as_f64()
                        .ok_or_else(|| invalid("numeric value must be finite"))?;
                    append(&mut out, b"D")?;
                    append(&mut out, &f.to_bits().to_be_bytes())?;
                }
            }
            Part::Value(Value::Array(a)) => {
                append(&mut out, b"A")?;
                append(&mut out, &(a.len() as u64).to_be_bytes())?;
                for value in a.iter().rev() {
                    checkpoint_work(context, work)?;
                    work += 1;
                    stack.push(Part::Value(value));
                }
            }
            Part::Value(Value::Object(o)) => {
                append(&mut out, b"O")?;
                append(&mut out, &(o.len() as u64).to_be_bytes())?;
                let mut entries: Vec<_> = o.iter().collect();
                let mut failure = None;
                entries.sort_unstable_by(|a, b| {
                    if failure.is_none() {
                        if let Err(error) = checkpoint_work(context, work) {
                            failure = Some(error);
                        }
                        work += 1;
                    }
                    a.0.cmp(b.0)
                });
                if let Some(error) = failure {
                    return Err(error);
                }
                for (key, value) in entries.into_iter().rev() {
                    checkpoint_work(context, work)?;
                    work += 1;
                    stack.push(Part::Value(value));
                    stack.push(Part::String(key));
                }
            }
        }
    }
    Ok(out)
}

// Structural equality uses the same exact numeric ordering as canonical keys.
pub(super) fn json_equal(
    left: &Value,
    right: &Value,
    context: &ExecutionContext<'_>,
    work: &mut usize,
) -> Result<bool, AppletError> {
    let mut pending = vec![(left, right)];
    while let Some((left, right)) = pending.pop() {
        checkpoint_work(context, *work)?;
        *work += 1;
        match (left, right) {
            (Value::Number(a), Value::Number(b)) if numeric_order(a, b)? == Ordering::Equal => {}
            (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
                for pair in a.iter().zip(b) {
                    checkpoint_work(context, *work)?;
                    *work += 1;
                    pending.push(pair);
                }
            }
            (Value::Object(a), Value::Object(b)) if a.len() == b.len() => {
                for (key, value) in a {
                    checkpoint_work(context, *work)?;
                    *work += 1;
                    let Some(other) = b.get(key) else {
                        return Ok(false);
                    };
                    pending.push((value, other));
                }
            }
            (Value::Number(_), Value::Number(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Object(_), Value::Object(_)) => return Ok(false),
            _ if left == right => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::applets::ExecutionBudgetExceeded;

    #[test]
    fn nested_equality_work_observes_cancellation() {
        let value = serde_json::json!([(0..4000).collect::<Vec<_>>()]);
        let calls = std::cell::Cell::new(0);
        let callback = || {
            calls.set(calls.get() + 1);
            if calls.get() > 2 {
                Err(ExecutionBudgetExceeded::Cancelled)
            } else {
                Ok(())
            }
        };
        let context = ExecutionContext::new(&callback);
        assert_eq!(
            equality_key(&value, &context).unwrap_err().code,
            "cancelled"
        );
        calls.set(0);
        assert_eq!(
            json_equal(&value, &value, &context, &mut 0)
                .unwrap_err()
                .code,
            "cancelled"
        );
    }
}

pub(super) fn check_size_and_depth(
    value: &Value,
    context: &ExecutionContext<'_>,
) -> Result<usize, AppletError> {
    // One walk checks the depth and adds up the exact length of the compact JSON
    // encoding, so the size limit needs no serialization pass over the document.
    measure_json(vec![(value, 0)], 0, context)
}

fn measure_json(
    mut pending: Vec<(&Value, usize)>,
    mut bytes: usize,
    context: &ExecutionContext<'_>,
) -> Result<usize, AppletError> {
    let mut work = 0usize;
    let too_large = || invalid(format!("JSON value exceeds the {MAX_BYTES}-byte limit"));
    if bytes.saturating_add(pending.len()) > MAX_BYTES {
        return Err(too_large());
    }
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
            Value::String(text) => escaped_length_checked(text, context)?,
            Value::Array(items) => {
                let minimum = 2usize
                    .saturating_add(items.len().saturating_sub(1))
                    .saturating_add(items.len());
                if bytes.saturating_add(pending.len()).saturating_add(minimum) > MAX_BYTES {
                    return Err(too_large());
                }
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
                let minimum = length.saturating_add(fields.len().saturating_mul(4));
                if bytes.saturating_add(pending.len()).saturating_add(minimum) > MAX_BYTES {
                    return Err(too_large());
                }
                for (key, item) in fields {
                    checkpoint_work(context, work)?;
                    work += 1;
                    // The quoted key and its colon.
                    length = length.saturating_add(escaped_length_checked(key, context)? + 1);
                    if bytes
                        .saturating_add(length)
                        .saturating_add(pending.len())
                        .saturating_add(1)
                        > MAX_BYTES
                    {
                        return Err(too_large());
                    }
                    pending.push((item, depth + 1));
                }
                length
            }
        });
        if bytes.saturating_add(pending.len()) > MAX_BYTES {
            return Err(too_large());
        }
    }
    context.checkpoint()?;
    if bytes > MAX_BYTES {
        return Err(invalid(format!(
            "JSON value exceeds the {MAX_BYTES}-byte limit"
        )));
    }
    Ok(bytes)
}

pub(super) fn escaped_length_checked(
    text: &str,
    context: &ExecutionContext<'_>,
) -> Result<usize, AppletError> {
    if text.len().saturating_add(2) > MAX_BYTES {
        return Err(invalid(format!(
            "JSON value exceeds the {MAX_BYTES}-byte limit"
        )));
    }
    let mut length = text.len() + 2;
    for chunk in text.as_bytes().chunks(4096) {
        if text.len() > 4096 {
            context.checkpoint()?;
        }
        for byte in chunk {
            length = length.saturating_add(match byte {
                b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 1,
                0x00..=0x1f => 5,
                _ => 0,
            });
        }
        if length > MAX_BYTES {
            return Err(invalid(format!(
                "JSON value exceeds the {MAX_BYTES}-byte limit"
            )));
        }
    }
    Ok(length)
}

/// The length of `text` as a quoted JSON string in serde_json's compact encoding:
/// `"` and `\` and the short control escapes take two bytes, other control bytes take
/// six (`\u00XX`), and every other byte, including non-ASCII UTF-8, is copied.
#[cfg(test)]
pub(super) fn escaped_length(text: &str) -> usize {
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
pub(super) fn number_length(number: &serde_json::Number) -> usize {
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
