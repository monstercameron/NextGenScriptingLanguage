//! Shared bounds, numeric ordering, and JSON equality for applet operations.
use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Number, Value};
use std::cmp::Ordering;

/// The one shared item bound, the same as the list, CSV and JSON applets use.
pub(super) const MAX_ITEMS: usize = super::list::ITEM_LIMIT;
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
        length = length.saturating_add(escape_extra(chunk));
        if length > MAX_BYTES {
            return Err(invalid(format!(
                "JSON value exceeds the {MAX_BYTES}-byte limit"
            )));
        }
    }
    Ok(length)
}

/// Strings at or above this length use the AVX2/NEON escape counter where the
/// CPU supports it; shorter strings stay on the scalar counter so dispatch
/// never costs more than the few bytes it would scan.
pub(super) const ESCAPE_VECTOR_THRESHOLD: usize = 64;

/// Extra bytes serde_json's compact encoding adds for `bytes`: `"` and `\`
/// and the short control escapes add one byte each, other control bytes add
/// five each (`\u00XX`), and every other byte is copied as-is.
fn escape_extra(bytes: &[u8]) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if bytes.len() >= ESCAPE_VECTOR_THRESHOLD && std::is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was just detected at runtime. `escape_extra_avx2`
            // only reads inside `bytes` with unaligned loads and counts the
            // tail scalarly, so no byte is missed or double-counted.
            return unsafe { escape_extra_avx2(bytes) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if bytes.len() >= ESCAPE_VECTOR_THRESHOLD {
            // SAFETY: NEON is guaranteed on aarch64. `escape_extra_neon`
            // only reads inside `bytes` with unaligned loads and counts the
            // tail scalarly, so no byte is missed or double-counted.
            return unsafe { escape_extra_neon(bytes) };
        }
    }
    escape_extra_scalar(bytes)
}

fn escape_extra_scalar(bytes: &[u8]) -> usize {
    let mut extra = 0usize;
    for byte in bytes {
        extra += match byte {
            b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 1,
            0x00..=0x1f => 5,
            _ => 0,
        };
    }
    extra
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn escape_extra_avx2(bytes: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let (blocks, remainder) = bytes.as_chunks::<32>();
    // SAFETY: the caller guarantees AVX2 is available (checked with runtime
    // feature detection on x86_64). Every intrinsic below runs under that
    // guarantee; every load reads exactly 32 bytes inside `bytes` via
    // `_mm256_loadu_si256`, which needs no alignment, and the remainder is
    // counted scalarly outside this block.
    let vector = unsafe {
        let quote = _mm256_set1_epi8(b'"' as i8);
        let backslash = _mm256_set1_epi8(b'\\' as i8);
        // `max_epu8(v, 0x1f) == 0x1f` exactly for bytes below 0x20, since the
        // maximum of an unsigned byte and 0x1f is 0x1f only in that case.
        let ctrl_ceiling = _mm256_set1_epi8(0x1f);
        let short = [
            _mm256_set1_epi8(0x08),
            _mm256_set1_epi8(0x09),
            _mm256_set1_epi8(0x0a),
            _mm256_set1_epi8(0x0c),
            _mm256_set1_epi8(0x0d),
        ];
        let mut quote_bs = 0usize;
        let mut ctrl = 0usize;
        let mut short_ctrl = 0usize;
        for block in blocks {
            // `as_chunks::<32>()` yields arrays of exactly 32 bytes, so this
            // unaligned load stays in bounds.
            let values = _mm256_loadu_si256(block.as_ptr() as *const __m256i);
            let is_quote_bs = _mm256_or_si256(
                _mm256_cmpeq_epi8(values, quote),
                _mm256_cmpeq_epi8(values, backslash),
            );
            quote_bs += _mm256_movemask_epi8(is_quote_bs).count_ones() as usize;
            let is_ctrl = _mm256_cmpeq_epi8(_mm256_max_epu8(values, ctrl_ceiling), ctrl_ceiling);
            ctrl += _mm256_movemask_epi8(is_ctrl).count_ones() as usize;
            let mut is_short = _mm256_cmpeq_epi8(values, short[0]);
            for target in &short[1..] {
                is_short = _mm256_or_si256(is_short, _mm256_cmpeq_epi8(values, *target));
            }
            short_ctrl += _mm256_movemask_epi8(is_short).count_ones() as usize;
        }
        // Short escapes cost one extra byte each; the remaining control bytes
        // cost five each. Every short control byte is also a control byte,
        // hence the subtraction below.
        quote_bs + short_ctrl + (ctrl - short_ctrl) * 5
    };
    vector + escape_extra_scalar(remainder)
}

#[cfg(target_arch = "aarch64")]
unsafe fn escape_extra_neon(bytes: &[u8]) -> usize {
    use std::arch::aarch64::*;
    let (blocks, remainder) = bytes.as_chunks::<16>();
    // SAFETY: NEON is part of the aarch64 baseline, so no runtime detection is
    // needed. Every intrinsic below runs under that guarantee; every load
    // reads exactly 16 bytes inside `bytes` via `vld1q_u8`, which needs no
    // alignment, and the remainder is counted scalarly outside this block.
    let vector = unsafe {
        let quote = vdupq_n_u8(b'"');
        let backslash = vdupq_n_u8(b'\\');
        let ctrl_limit = vdupq_n_u8(0x20);
        let short = [
            vdupq_n_u8(0x08),
            vdupq_n_u8(0x09),
            vdupq_n_u8(0x0a),
            vdupq_n_u8(0x0c),
            vdupq_n_u8(0x0d),
        ];
        let mut quote_bs = 0usize;
        let mut ctrl = 0usize;
        let mut short_ctrl = 0usize;
        for block in blocks {
            // `as_chunks::<16>()` yields arrays of exactly 16 bytes, so this
            // load stays in bounds.
            let values = vld1q_u8(block.as_ptr());
            let is_quote_bs = vorrq_u8(vceqq_u8(values, quote), vceqq_u8(values, backslash));
            let is_ctrl = vcltq_u8(values, ctrl_limit);
            let mut is_short = vceqq_u8(values, short[0]);
            for target in &short[1..] {
                is_short = vorrq_u8(is_short, vceqq_u8(values, *target));
            }
            // Each comparison mask holds 0xff per matching lane; shifting
            // right by 7 turns it into 1 per lane so the sum is the count.
            quote_bs += vaddvq_u8(vshrq_n_u8::<7>(is_quote_bs)) as usize;
            ctrl += vaddvq_u8(vshrq_n_u8::<7>(is_ctrl)) as usize;
            short_ctrl += vaddvq_u8(vshrq_n_u8::<7>(is_short)) as usize;
        }
        // Short escapes cost one extra byte each; the remaining control bytes
        // cost five each. Every short control byte is also a control byte,
        // hence the subtraction below.
        quote_bs + short_ctrl + (ctrl - short_ctrl) * 5
    };
    vector + escape_extra_scalar(remainder)
}

/// The vector path selected for `bytes` on this CPU, or `None` where no SIMD
/// path applies, so parity tests can exercise each detected ISA path directly.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
pub(super) fn escape_extra_vector(bytes: &[u8]) -> Option<usize> {
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was just detected at runtime; the callee only reads
        // inside `bytes` with unaligned loads.
        Some(unsafe { escape_extra_avx2(bytes) })
    } else {
        None
    }
}

/// The vector path selected for `bytes` on this CPU. NEON is guaranteed on
/// aarch64, so this always applies there.
#[cfg(test)]
#[cfg(target_arch = "aarch64")]
pub(super) fn escape_extra_vector(bytes: &[u8]) -> Option<usize> {
    // SAFETY: NEON is part of the aarch64 baseline; the callee only reads
    // inside `bytes` with unaligned loads.
    Some(unsafe { escape_extra_neon(bytes) })
}

/// No SIMD path applies on this target, so parity tests fall back to the
/// scalar counter.
#[cfg(test)]
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(super) fn escape_extra_vector(_bytes: &[u8]) -> Option<usize> {
    None
}

/// The length of `text` as a quoted JSON string in serde_json's compact encoding:
/// `"` and `\` and the short control escapes take two bytes, other control bytes take
/// six (`\u00XX`), and every other byte, including non-ASCII UTF-8, is copied.
#[cfg(test)]
pub(super) fn escaped_length(text: &str) -> usize {
    text.len() + escape_extra(text.as_bytes()) + 2
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
