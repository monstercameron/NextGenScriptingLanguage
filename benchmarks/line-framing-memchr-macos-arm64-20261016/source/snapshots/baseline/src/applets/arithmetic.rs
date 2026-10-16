use super::{failed, invalid, required_string, AppletError};
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;

fn number<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a Number, AppletError> {
    args.get(name)
        .and_then(Value::as_number)
        .ok_or_else(|| invalid(format!("args.{name} must be a JSON number")))
}

fn as_integer(value: &Number) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

fn from_integer(value: i128) -> Result<Value, AppletError> {
    Number::from_i128(value)
        .map(Value::Number)
        .ok_or_else(|| failed("overflow", "integer result exceeds JSON's supported range"))
}

fn from_float(value: f64) -> Result<Value, AppletError> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| failed("non_finite", "numeric result is not finite"))
}

pub(super) fn run(operation: &str, args: &Map<String, Value>) -> Result<Value, AppletError> {
    let left = number(args, "left")?;
    let right = number(args, "right")?;

    if operation == "number.compare" {
        let ordering = match (as_integer(left), as_integer(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            _ => left
                .as_f64()
                .zip(right.as_f64())
                .and_then(|(left, right)| left.partial_cmp(&right))
                .ok_or_else(|| invalid("number.compare requires finite numeric operands"))?,
        };
        return Ok(Value::from(match ordering {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }));
    }

    if let (Some(left), Some(right)) = (as_integer(left), as_integer(right)) {
        let result = match operation {
            "number.add" => left.checked_add(right),
            "number.subtract" => left.checked_sub(right),
            "number.multiply" => left.checked_mul(right),
            "number.divide" => None,
            _ => unreachable!("operation dispatch is closed"),
        };
        if operation != "number.divide" {
            return result
                .ok_or_else(|| failed("overflow", "integer arithmetic overflowed"))
                .and_then(from_integer);
        }
    }

    let left = left
        .as_f64()
        .ok_or_else(|| invalid("args.left must be a finite number"))?;
    let right = right
        .as_f64()
        .ok_or_else(|| invalid("args.right must be a finite number"))?;
    if operation == "number.divide" && right == 0.0 {
        return Err(invalid("division by zero"));
    }
    let result = match operation {
        "number.add" => left + right,
        "number.subtract" => left - right,
        "number.multiply" => left * right,
        "number.divide" => left / right,
        _ => unreachable!("operation dispatch is closed"),
    };
    from_float(result)
}

pub(super) fn to_text(args: &Map<String, Value>) -> Result<Value, AppletError> {
    // serde_json::Number formatting is deterministic and locale independent.
    // It intentionally renders the JSON number representation; this does not
    // coerce strings or promise exact decimal arithmetic.
    Ok(Value::String(number(args, "value")?.to_string()))
}

pub(super) fn parse_integer(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let text = match args.get("text") {
        Some(Value::String(value)) => value.as_str(),
        Some(_) => return Err(invalid("args.text must be a string")),
        None => std::str::from_utf8(input)
            .map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))?,
    };
    let base = match args.get("base") {
        None => 10,
        Some(value) => value
            .as_u64()
            .filter(|base| (2..=36).contains(base))
            .ok_or_else(|| invalid("args.base must be an integer from 2 through 36"))?
            as u32,
    };
    let trim = match args.get("trim_ascii_whitespace") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("args.trim_ascii_whitespace must be a boolean")),
    };
    let text = if trim {
        text.trim_matches(|character: char| character.is_ascii_whitespace())
    } else {
        text
    };
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty()
        || !digits.bytes().all(|byte| {
            let Some(digit) = (byte as char).to_digit(base) else {
                return false;
            };
            digit < base
        })
    {
        return Err(invalid(format!("args.text is not a base-{base} integer")));
    }
    let magnitude = u64::from_str_radix(digits, base)
        .map_err(|_| failed("overflow", "integer is outside the signed 64-bit range"))?;
    let value = if negative {
        let limit = i64::MAX as u64 + 1;
        if magnitude > limit {
            return Err(failed(
                "overflow",
                "integer is outside the signed 64-bit range",
            ));
        }
        if magnitude == limit {
            i64::MIN
        } else {
            -(magnitude as i64)
        }
    } else {
        i64::try_from(magnitude)
            .map_err(|_| failed("overflow", "integer is outside the signed 64-bit range"))?
    };
    Ok(Value::from(value))
}

pub(super) fn rgb_to_hex(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let channel = |name: &str| -> Result<u8, AppletError> {
        let value = args
            .get(name)
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid(format!("args.{name} must be an integer from 0 through 255")))?;
        u8::try_from(value)
            .map_err(|_| invalid(format!("args.{name} must be an integer from 0 through 255")))
    };
    let red = channel("red")?;
    let green = channel("green")?;
    let blue = channel("blue")?;
    let uppercase = match args.get("uppercase") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("args.uppercase must be a boolean")),
    };
    let color = if uppercase {
        format!("#{red:02X}{green:02X}{blue:02X}")
    } else {
        format!("#{red:02x}{green:02x}{blue:02x}")
    };
    // Formatting uses ASCII digits and is independent of process locale.
    Ok(Value::String(color))
}

pub(super) fn parse_rgb(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let fallback = normalized_hex(required_string(args, "default_hex")?)
        .ok_or_else(|| invalid("args.default_hex must contain six ASCII hexadecimal digits"))?;
    let text = match args.get("text") {
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err(invalid("args.text must be a string")),
        None => std::str::from_utf8(input).ok(),
    };
    let Some(text) = text else {
        return Ok(serde_json::json!({"valid": false, "hex": fallback}));
    };
    let channels = text.split(',').collect::<Vec<_>>();
    if channels.len() != 3 {
        return Ok(serde_json::json!({"valid": false, "hex": fallback}));
    }
    let parsed = channels
        .into_iter()
        .map(|channel| {
            let channel = channel.trim_matches(|character| {
                matches!(
                    character,
                    ' ' | '\t' | '\n' | '\r' | '\u{000b}' | '\u{000c}'
                )
            });
            (!channel.is_empty() && channel.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| channel.parse::<u16>().ok())
                .flatten()
                .filter(|value| *value <= 255)
                .map(|value| value as u8)
        })
        .collect::<Option<Vec<_>>>();
    let Some([red, green, blue]) = parsed.as_deref() else {
        return Ok(serde_json::json!({"valid": false, "hex": fallback}));
    };
    Ok(serde_json::json!({
        "valid": true,
        "hex": format!("{red:02x}{green:02x}{blue:02x}"),
    }))
}

pub(super) fn parse_rgb_result(
    args: &Map<String, Value>,
    input: &[u8],
) -> Result<Value, AppletError> {
    let parsed = parse_rgb(args, input)?;
    if parsed.get("valid").and_then(Value::as_bool) != Some(true) {
        return Err(failed(
            "invalid_color",
            "RGB input must contain three decimal channels from 0 through 255",
        ));
    }
    Ok(parsed)
}

fn normalized_hex(value: &str) -> Option<String> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    (hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}
