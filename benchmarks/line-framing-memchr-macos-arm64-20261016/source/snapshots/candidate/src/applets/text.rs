use super::{checkpoint_work, invalid, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};

const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MATCHES: usize = 100_000;

fn text_input<'a>(args: &'a Map<String, Value>, input: &'a [u8]) -> Result<&'a str, AppletError> {
    if let Some(value) = args.get("text") {
        return value
            .as_str()
            .ok_or_else(|| invalid("args.text must be a string"));
    }
    std::str::from_utf8(input).map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))
}

pub(super) fn trim_final_lf(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    Ok(Value::String(
        text.strip_suffix('\n').unwrap_or(text).to_owned(),
    ))
}

pub(super) fn concat(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let values = args
        .get("parts")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.parts must be an array of strings"))?;
    if values.len() > MAX_MATCHES {
        return Err(invalid(format!(
            "args.parts exceeds the {MAX_MATCHES}-item limit"
        )));
    }
    let separator = match args.get("separator") {
        None => "",
        Some(Value::String(value)) => value.as_str(),
        Some(_) => return Err(invalid("args.separator must be a string")),
    };
    let mut result = String::new();
    for (index, value) in values.iter().enumerate() {
        checkpoint_work(context, index)?;
        let part = value
            .as_str()
            .ok_or_else(|| invalid(format!("args.parts[{index}] must be a string")))?;
        let separator_bytes = if index > 0 { separator.len() } else { 0 };
        if result
            .len()
            .checked_add(separator_bytes)
            .and_then(|length| length.checked_add(part.len()))
            .is_none_or(|length| length > MAX_TEXT_BYTES)
        {
            return Err(invalid(format!(
                "text.concat output exceeds the {MAX_TEXT_BYTES}-byte limit"
            )));
        }
        if index > 0 {
            result.push_str(separator);
        }
        result.push_str(part);
    }
    Ok(Value::String(result))
}

pub(super) fn matches(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    let needle = required_string(args, "needle")?;
    check_text_size(text.len())?;
    check_text_size(needle.len())?;
    let mut offsets = Vec::new();
    if needle.is_empty() {
        for (index, (offset, _)) in text.char_indices().enumerate() {
            checkpoint_work(context, index)?;
            push_match(&mut offsets, offset)?;
        }
        push_match(&mut offsets, text.len())?;
    } else if needle.len() <= text.len() {
        find_overlapping(text.as_bytes(), needle.as_bytes(), &mut offsets, context)?;
    }
    let count = offsets.len();
    let offsets: Vec<Value> = offsets
        .into_iter()
        .enumerate()
        .map(|(index, offset)| {
            checkpoint_work(context, index)?;
            Ok(Value::from(offset))
        })
        .collect::<Result<_, AppletError>>()?;
    Ok(serde_json::json!({
        "matches": count != 0,
        "count": count,
        "byte_offsets": offsets,
    }))
}

fn push_match(offsets: &mut Vec<usize>, offset: usize) -> Result<(), AppletError> {
    if offsets.len() == MAX_MATCHES {
        return Err(invalid(format!(
            "text.matches output exceeds the {MAX_MATCHES}-match limit"
        )));
    }
    offsets.push(offset);
    Ok(())
}

/// Find overlapping byte matches in linear time. UTF-8 continuation bytes
/// cannot begin a valid UTF-8 needle, so byte matches have the same start
/// offsets as the prior character-boundary scan.
fn find_overlapping(
    text: &[u8],
    needle: &[u8],
    offsets: &mut Vec<usize>,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    // Text is limited to 16 MiB, so u32 safely holds every prefix length and
    // uses half the storage of a platform-sized index on 64-bit hosts.
    let mut prefix = vec![0u32; needle.len()];
    let mut work = 0usize;
    for index in 1..needle.len() {
        tick(context, &mut work)?;
        let mut matched = prefix[index - 1] as usize;
        while matched > 0 && needle[index] != needle[matched] {
            tick(context, &mut work)?;
            matched = prefix[matched - 1] as usize;
        }
        if needle[index] == needle[matched] {
            matched += 1;
        }
        prefix[index] = matched as u32;
    }

    let mut matched = 0usize;
    for (index, byte) in text.iter().copied().enumerate() {
        tick(context, &mut work)?;
        while matched > 0 && byte != needle[matched] {
            tick(context, &mut work)?;
            matched = prefix[matched - 1] as usize;
        }
        if byte == needle[matched] {
            matched += 1;
            if matched == needle.len() {
                push_match(offsets, index + 1 - needle.len())?;
                matched = prefix[matched - 1] as usize;
            }
        }
    }
    Ok(())
}

fn tick(context: &ExecutionContext<'_>, work: &mut usize) -> Result<(), AppletError> {
    *work += 1;
    if *work == 256 {
        *work = 0;
        context.checkpoint()?;
    }
    Ok(())
}

fn check_text_size(length: usize) -> Result<(), AppletError> {
    if length > MAX_TEXT_BYTES {
        return Err(invalid(format!(
            "text exceeds the {MAX_TEXT_BYTES}-byte limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_matcher_agrees_with_character_boundary_scan() {
        let context = ExecutionContext::unrestricted();
        for (text, needle) in [
            ("ababa", "aba"),
            ("abababacababac", "ababac"),
            ("aaaaaaaaabaaaaaaab", "aaaaaaab"),
            ("é😊é😊é", "😊é"),
            ("αβαβαβ", "αβ"),
            ("short", "longer needle"),
        ] {
            let mut actual = Vec::new();
            if needle.len() <= text.len() {
                find_overlapping(text.as_bytes(), needle.as_bytes(), &mut actual, &context)
                    .unwrap();
            }
            let expected = text
                .char_indices()
                .filter_map(|(offset, _)| text[offset..].starts_with(needle).then_some(offset))
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "text={text:?}, needle={needle:?}");
        }
    }
}
