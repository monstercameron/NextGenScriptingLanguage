use super::{failed, invalid, required_string, AppletError};
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;

fn number<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a Number, AppletError> {
    args.get(name)
        .and_then(Value::as_number)
        .ok_or_else(|| invalid(format!("args.{name} must be a JSON number")))
}

fn precision(args: &Map<String, Value>, default: u64, maximum: u64) -> Result<usize, AppletError> {
    let precision = match args.get("precision") {
        None => default,
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= maximum)
            .ok_or_else(|| {
                invalid(format!(
                    "args.precision must be an integer from 0 through {maximum}"
                ))
            })?,
    };
    Ok(precision as usize)
}

fn rounding_digits(args: &Map<String, Value>) -> Result<i64, AppletError> {
    match args.get("digits") {
        None => Ok(0),
        Some(value) => value
            .as_i64()
            .ok_or_else(|| invalid("args.digits must be a signed 64-bit integer")),
    }
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

// Round the shortest decimal representation of the JSON float, with ties away
// from zero. Decimal digit manipulation avoids overflowing a floating-point scale
// or losing a tie while multiplying by a power of ten.
fn round_float(value: f64, digits: i64) -> Result<Value, AppletError> {
    if !value.is_finite() {
        return Err(invalid("args.value must be finite"));
    }
    let representation = value.abs().to_string();
    let (mantissa, exponent) = representation.split_once(['e', 'E']).map_or(
        (representation.as_str(), 0),
        |(mantissa, exponent)| {
            // The exponent comes from f64's own formatter, so it is always valid.
            (mantissa, exponent.parse::<i32>().expect("float exponent"))
        },
    );
    let decimal_position = mantissa.find('.').unwrap_or(mantissa.len()) as i32 + exponent;
    let mut coefficient: Vec<u8> = mantissa.bytes().filter(|byte| *byte != b'.').collect();
    let cutoff = i64::from(decimal_position) + digits;
    if cutoff >= coefficient.len() as i64 {
        return from_float(value);
    }
    if cutoff < 0 {
        return from_float(0.0f64.copysign(value));
    }
    let cutoff = cutoff as usize;
    let increment = coefficient[cutoff] >= b'5';
    coefficient.truncate(cutoff);
    if increment {
        let mut carry = true;
        for digit in coefficient.iter_mut().rev() {
            if *digit == b'9' {
                *digit = b'0';
            } else {
                *digit += 1;
                carry = false;
                break;
            }
        }
        if carry {
            coefficient.insert(0, b'1');
        }
    }
    if coefficient.is_empty() {
        return from_float(0.0f64.copysign(value));
    }
    let coefficient = String::from_utf8(coefficient).expect("decimal digits are ASCII");
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let rounded = format!("{sign}{coefficient}e{}", -digits)
        .parse::<f64>()
        .map_err(|_| {
            failed(
                "overflow",
                "rounded number is outside the finite float range",
            )
        })?;
    if !rounded.is_finite() {
        return Err(failed(
            "overflow",
            "rounded number is outside the finite float range",
        ));
    }
    from_float(rounded)
}

pub(super) fn run(operation: &str, args: &Map<String, Value>) -> Result<Value, AppletError> {
    if operation == "number.format" {
        let value = number(args, "value")?;
        let precision = precision(args, 0, 15)?;
        let mut text = if let Some(integer) = as_integer(value) {
            let negative = integer < 0;
            let digits = integer.unsigned_abs().to_string();
            let grouped = match args
                .get("grouping")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                true => group_digits(&digits),
                false => digits,
            };
            let sign = if negative { "-" } else { "" };
            if precision == 0 {
                format!("{sign}{grouped}")
            } else {
                format!("{sign}{grouped}.{}", "0".repeat(precision))
            }
        } else {
            let value = value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| invalid("args.value must be finite"))?;
            let formatted = format!("{value:.precision$}");
            if args
                .get("grouping")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                group_formatted(&formatted)
            } else {
                formatted
            }
        };
        if args
            .get("percent")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            text.push('%');
        }
        return Ok(Value::String(text));
    }
    if operation == "number.format_size" {
        let value = number(args, "value")?
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .ok_or_else(|| invalid("args.value must be a nonnegative finite number"))?;
        let system = args.get("system").and_then(Value::as_str).unwrap_or("iec");
        let (base, units): (f64, &[&str]) = match system {
            "iec" => (1024.0, &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"]),
            "si" => (1000.0, &["B", "kB", "MB", "GB", "TB", "PB", "EB"]),
            _ => return Err(invalid("args.system must be si or iec")),
        };
        let precision = precision(args, 1, 6)?;
        let mut scaled = value;
        let mut unit = 0;
        while scaled >= base && unit + 1 < units.len() {
            scaled /= base;
            unit += 1;
        }
        return Ok(Value::String(format!(
            "{scaled:.precision$} {}",
            units[unit]
        )));
    }
    if operation == "number.divmod" {
        let left = as_integer(number(args, "left")?)
            .ok_or_else(|| invalid("args.left must be an integer"))?;
        let right = as_integer(number(args, "right")?)
            .ok_or_else(|| invalid("args.right must be an integer"))?;
        if right == 0 {
            return Err(invalid("division by zero"));
        }
        let quotient = left
            .checked_div(right)
            .ok_or_else(|| failed("overflow", "integer arithmetic overflowed"))?;
        let remainder = left
            .checked_rem(right)
            .ok_or_else(|| failed("overflow", "integer arithmetic overflowed"))?;
        let quotient = from_integer(quotient)?;
        let remainder = from_integer(remainder)?;
        return Ok(serde_json::json!({"quotient":quotient,"remainder":remainder}));
    }
    if matches!(operation, "number.min" | "number.max") {
        let values = args
            .get("values")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| invalid("args.values must be a nonempty numeric list"))?;
        let mut best = values[0]
            .as_number()
            .ok_or_else(|| invalid("args.values must contain numbers"))?;
        for value in &values[1..] {
            let candidate = value
                .as_number()
                .ok_or_else(|| invalid("args.values must contain numbers"))?;
            let ordering = super::shared::numeric_order(candidate, best)?;
            if (operation == "number.min" && ordering.is_lt())
                || (operation == "number.max" && ordering.is_gt())
            {
                best = candidate;
            }
        }
        return Ok(Value::Number(best.clone()));
    }
    if matches!(
        operation,
        "number.floor"
            | "number.ceil"
            | "number.trunc"
            | "number.round"
            | "number.abs"
            | "number.sign"
    ) {
        let value = number(args, "value")?;
        if let Some(integer) = as_integer(value) {
            let out = match operation {
                "number.abs" => integer
                    .checked_abs()
                    .ok_or_else(|| failed("overflow", "absolute value overflowed"))?,
                "number.sign" => integer.signum(),
                "number.round" => {
                    let digits = rounding_digits(args)?;
                    if !(-38..=38).contains(&digits) {
                        return Err(invalid(
                            "integer rounding digits must be between -38 and 38",
                        ));
                    }
                    if digits >= 0 {
                        integer
                    } else {
                        let divisor = 10i128
                            .checked_pow((-digits) as u32)
                            .ok_or_else(|| failed("overflow", "rounding scale overflowed"))?;
                        let quotient = integer / divisor;
                        let remainder = integer % divisor;
                        let rounded = if remainder.unsigned_abs() * 2 >= divisor as u128 {
                            quotient + integer.signum()
                        } else {
                            quotient
                        };
                        rounded
                            .checked_mul(divisor)
                            .ok_or_else(|| failed("overflow", "rounded integer overflowed"))?
                    }
                }
                _ => integer,
            };
            return from_integer(out);
        }
        let value = value
            .as_f64()
            .ok_or_else(|| invalid("args.value must be finite"))?;
        let out = match operation {
            "number.floor" => value.floor(),
            "number.ceil" => value.ceil(),
            "number.trunc" => value.trunc(),
            "number.round" => {
                let digits = rounding_digits(args)?;
                if !(-308..=308).contains(&digits) {
                    return Err(invalid("args.digits must be between -308 and 308"));
                }
                return round_float(value, digits);
            }
            "number.abs" => value.abs(),
            "number.sign" => value.signum(),
            _ => value,
        };
        return from_float(out);
    }
    if operation == "number.clamp" {
        let value = number(args, "value")?;
        let min = number(args, "min")?;
        let max = number(args, "max")?;
        if super::shared::numeric_order(min, max)?.is_gt() {
            return Err(invalid("args.min must not exceed args.max"));
        }
        return Ok(Value::Number(
            if super::shared::numeric_order(value, min)?.is_lt() {
                min.clone()
            } else if super::shared::numeric_order(value, max)?.is_gt() {
                max.clone()
            } else {
                value.clone()
            },
        ));
    }
    if matches!(operation, "number.int_divide" | "number.modulo") {
        let left = as_integer(number(args, "left")?)
            .ok_or_else(|| invalid("args.left must be an integer"))?;
        let right = as_integer(number(args, "right")?)
            .ok_or_else(|| invalid("args.right must be an integer"))?;
        if right == 0 {
            return Err(invalid("division by zero"));
        }
        let result = if operation == "number.int_divide" {
            left.checked_div(right)
        } else {
            left.checked_rem(right)
        }
        .ok_or_else(|| failed("overflow", "integer arithmetic overflowed"))?;
        return from_integer(result);
    }
    let left = number(args, "left")?;
    let right = number(args, "right")?;

    if operation == "number.compare" {
        let ordering = super::shared::numeric_order(left, right)?;
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

fn group_digits(digits: &str) -> String {
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output
}

fn group_formatted(value: &str) -> String {
    let (sign, rest) = value
        .strip_prefix('-')
        .map_or(("", value), |rest| ("-", rest));
    let (whole, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    let mut output = format!("{sign}{}", group_digits(whole));
    if !fraction.is_empty() {
        output.push('.');
        output.push_str(fraction);
    }
    output
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

#[cfg(test)]
mod round_tests {
    use super::round_float;

    #[test]
    fn non_finite_float_is_rejected() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(round_float(value, 0).unwrap_err().code, "invalid_request");
        }
    }
}
