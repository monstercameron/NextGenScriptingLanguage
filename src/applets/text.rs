use super::{checkpoint_work, invalid, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};

const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MATCHES: usize = 100_000;
const MAX_LIST_ITEMS: usize = 100_000;

pub(super) fn text_input<'a>(
    args: &'a Map<String, Value>,
    input: &'a [u8],
) -> Result<&'a str, AppletError> {
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

/// Split UTF-8 text into lines. One final LF is a terminator rather than an
/// empty final line, and one CR before each line break (or at the end of an
/// unterminated final line) is removed. Empty text gives an empty list.
pub(super) fn lines(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    if text.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let content = text.strip_suffix('\n').unwrap_or(text);
    let line_count =
        crate::bytes::count_newlines(text.as_bytes()) + usize::from(!text.ends_with('\n'));
    // Reserve at most the item limit: the count comes from untrusted input.
    let mut items = Vec::with_capacity(line_count.min(MAX_LIST_ITEMS));
    for (index, line) in content.split('\n').enumerate() {
        checkpoint_work(context, index)?;
        push_list_item(
            &mut items,
            line.strip_suffix('\r').unwrap_or(line),
            "text.lines",
        )?;
    }
    Ok(Value::Array(items))
}

/// Split text on a literal separator, or on NUL when `nul` is true. One final
/// separator terminates the last item instead of adding an empty item, so a
/// NUL-terminated `find -print0` stream yields its paths. Empty text gives an
/// empty list. The separator cannot be empty.
pub(super) fn split(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    let nul = match args.get("nul") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("args.nul must be a boolean")),
    };
    let separator = match (args.get("separator"), nul) {
        (None, true) => "\0",
        (Some(_), true) => {
            return Err(invalid(
                "text.split accepts either args.separator or args.nul true, not both",
            ))
        }
        (Some(Value::String(separator)), false) => separator.as_str(),
        (Some(_), false) => return Err(invalid("args.separator must be a string")),
        (None, false) => {
            return Err(invalid(
                "text.split requires args.separator or args.nul true",
            ))
        }
    };
    if separator.is_empty() {
        return Err(invalid("args.separator cannot be empty"));
    }
    if text.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let mut items = Vec::new();
    let mut split = text.split(separator).peekable();
    let mut index = 0;
    while let Some(item) = split.next() {
        if item.is_empty() && split.peek().is_none() {
            continue;
        }
        checkpoint_work(context, index)?;
        push_list_item(&mut items, item, "text.split")?;
        index += 1;
    }
    Ok(Value::Array(items))
}

fn push_list_item(items: &mut Vec<Value>, item: &str, operation: &str) -> Result<(), AppletError> {
    if items.len() == MAX_LIST_ITEMS {
        return Err(invalid(format!(
            "{operation} output exceeds the {MAX_LIST_ITEMS}-item limit"
        )));
    }
    items.push(Value::String(item.to_owned()));
    Ok(())
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

fn required_text<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, AppletError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("args.{name} must be a string")))
}

fn text_output(value: String) -> Result<Value, AppletError> {
    check_text_size(value.len())?;
    Ok(Value::String(value))
}

pub(super) fn shape(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    let unit = match args.get("unit") {
        None => "chars",
        Some(Value::String(unit)) => unit.as_str(),
        Some(_) => return Err(invalid("args.unit must be a string")),
    };
    let units = match unit {
        "chars" => text.chars().count(),
        "bytes" => text.len(),
        _ => {
            return Err(invalid(
                "unit must be chars or bytes (graphemes are unavailable)",
            ))
        }
    };
    match operation {
        "text.trim" => {
            let mode = match args.get("mode") {
                None => "both",
                Some(Value::String(mode)) => mode.as_str(),
                Some(_) => return Err(invalid("args.mode must be a string")),
            };
            if args.get("chars").is_some_and(|value| !value.is_string()) {
                return Err(invalid("args.chars must be a string"));
            }
            let chars = args.get("chars").and_then(Value::as_str);
            let trimmed = |c: char| chars.map_or(c.is_whitespace(), |set| set.contains(c));
            let value = match mode {
                "both" => text.trim_matches(trimmed),
                "start" => text.trim_start_matches(trimmed),
                "end" => text.trim_end_matches(trimmed),
                _ => return Err(invalid("mode must be both, start, or end")),
            };
            Ok(Value::String(value.to_owned()))
        }
        "text.length" => Ok(Value::from(units)),
        "text.starts_with" | "text.ends_with" => {
            let field = if operation == "text.starts_with" {
                "prefix"
            } else {
                "suffix"
            };
            let part = required_text(args, field)?;
            Ok(Value::Bool(if operation == "text.starts_with" {
                text.starts_with(part)
            } else {
                text.ends_with(part)
            }))
        }
        "text.contains_any" => {
            let needles = args
                .get("needles")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("args.needles must be a list of strings"))?;
            if needles.len() > MAX_LIST_ITEMS {
                return Err(invalid("args.needles exceeds the item limit"));
            }
            for (index, needle) in needles.iter().enumerate() {
                checkpoint_work(context, index)?;
                let needle = needle
                    .as_str()
                    .ok_or_else(|| invalid(format!("args.needles[{index}] must be a string")))?;
                if text.contains(needle) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        "text.truncate" => {
            let width = args
                .get("width")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("args.width must be a nonnegative integer"))?
                as usize;
            let ellipsis = args.get("ellipsis").and_then(Value::as_str).unwrap_or("…");
            let chars = text.chars().collect::<Vec<_>>();
            if chars.len() <= width {
                return Ok(Value::String(text.to_owned()));
            }
            let ending = ellipsis.chars().count();
            if ending > width {
                return Err(invalid("ellipsis is wider than args.width"));
            }
            let mut output = chars[..width - ending].iter().collect::<String>();
            output.push_str(ellipsis);
            text_output(output)
        }
        "text.substring" => {
            let start = args
                .get("start")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("args.start must be an integer"))?;
            let length = args
                .get("length")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("args.length must be a nonnegative integer"))?
                as usize;
            if args.get("unit").and_then(Value::as_str) == Some("bytes") {
                let start = if start < 0 {
                    text.len().checked_sub(start.unsigned_abs() as usize)
                } else {
                    Some(start as usize)
                }
                .ok_or_else(|| invalid("substring start is out of range"))?;
                let end = start
                    .checked_add(length)
                    .filter(|end| *end <= text.len())
                    .ok_or_else(|| invalid("substring length is out of range"))?;
                return text
                    .get(start..end)
                    .map(|slice| Value::String(slice.to_owned()))
                    .ok_or_else(|| {
                        invalid("substring byte indexes must be UTF-8 character boundaries")
                    });
            }
            let chars = text.chars().collect::<Vec<_>>();
            let start = if start < 0 {
                chars.len().checked_sub(start.unsigned_abs() as usize)
            } else {
                Some(start as usize)
            }
            .ok_or_else(|| invalid("substring start is out of range"))?;
            if start > chars.len() {
                return Err(invalid("substring start is out of range"));
            }
            let end = start
                .checked_add(length)
                .filter(|end| *end <= chars.len())
                .ok_or_else(|| invalid("substring length is out of range"))?;
            Ok(Value::String(chars[start..end].iter().collect()))
        }
        "text.pad" => {
            if args
                .get("display_width")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(invalid("args.display_width must be a boolean"));
            }
            if args.get("display_width").and_then(Value::as_bool) == Some(true) {
                return Err(invalid(
                    "display-width padding is not available in this build",
                ));
            }
            let width = args
                .get("width")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("args.width must be a nonnegative integer"))?
                as usize;
            let side = args.get("side").and_then(Value::as_str).unwrap_or("right");
            let fill = args.get("fill").and_then(Value::as_str).unwrap_or(" ");
            let fill_chars = fill.chars().collect::<Vec<_>>();
            if fill_chars.is_empty() {
                return Err(invalid("args.fill cannot be empty"));
            }
            let count = width.saturating_sub(text.chars().count());
            if count > MAX_TEXT_BYTES {
                return Err(invalid("text.pad output exceeds the text limit"));
            }
            let padding = std::iter::repeat_n(fill_chars.as_slice(), count)
                .flatten()
                .take(count)
                .collect::<String>();
            let value = match side {
                "left" => format!("{padding}{text}"),
                "right" => format!("{text}{padding}"),
                _ => return Err(invalid("side must be left or right")),
            };
            text_output(value)
        }
        "text.repeat" => {
            let count = args
                .get("count")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("args.count must be a nonnegative integer"))?
                as usize;
            if text
                .len()
                .checked_mul(count)
                .is_none_or(|size| size > MAX_TEXT_BYTES)
            {
                return Err(invalid("text.repeat output exceeds the text limit"));
            }
            text_output(text.repeat(count))
        }
        _ => Err(invalid("unknown text shaping operation")),
    }
}

/// Return `text`, or `default` when `text` is empty. Empty means zero bytes:
/// a value of only spaces is not empty.
pub(super) fn default_if_empty(
    args: &Map<String, Value>,
    input: &[u8],
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    let default = required_text(args, "default")?;
    check_text_size(default.len())?;
    text_output(if text.is_empty() { default } else { text }.to_owned())
}

/// Replace every non-overlapping occurrence of `from`, scanning left to right.
/// `from` cannot be empty, which would otherwise match between every character.
pub(super) fn replace(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    let from = required_text(args, "old")?;
    let to = required_text(args, "new")?;
    if from.is_empty() {
        return Err(invalid("args.old cannot be empty"));
    }
    check_text_size(text.len())?;
    check_text_size(to.len())?;
    let mut result = String::new();
    let mut copied = 0;
    for (index, (offset, _)) in text.match_indices(from).enumerate() {
        checkpoint_work(context, index)?;
        result.push_str(&text[copied..offset]);
        result.push_str(to);
        check_text_size(result.len())?;
        copied = offset + from.len();
    }
    result.push_str(&text[copied..]);
    text_output(result)
}

/// Remove `prefix` when the text starts with it; otherwise return the text.
pub(super) fn strip_prefix(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    let prefix = required_text(args, "prefix")?;
    check_text_size(text.len())?;
    check_text_size(prefix.len())?;
    text_output(text.strip_prefix(prefix).unwrap_or(text).to_owned())
}

/// Remove `suffix` when the text ends with it; otherwise return the text.
pub(super) fn strip_suffix(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    let suffix = required_text(args, "suffix")?;
    check_text_size(text.len())?;
    check_text_size(suffix.len())?;
    text_output(text.strip_suffix(suffix).unwrap_or(text).to_owned())
}

/// Unicode default lowercase mapping. It is locale-independent, so Turkish
/// dotted and dotless i are not special-cased.
pub(super) fn lower(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    text_output(text_input(args, input)?.to_lowercase())
}

/// Unicode default uppercase mapping. It is locale-independent.
pub(super) fn upper(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    text_output(text_input(args, input)?.to_uppercase())
}

fn tick(context: &ExecutionContext<'_>, work: &mut usize) -> Result<(), AppletError> {
    *work += 1;
    if *work == 256 {
        *work = 0;
        context.checkpoint()?;
    }
    Ok(())
}

pub(super) fn check_text_size(length: usize) -> Result<(), AppletError> {
    super::shared::check_bytes(length)
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

    #[test]
    fn case_conversion_matches_unicode_case_mapping() {
        for text in ["", "ascii TEXT 123", "Straße İΣ", "éclair ıß"] {
            let args = Map::from_iter([("text".to_owned(), Value::String(text.to_owned()))]);
            assert_eq!(
                upper(&args, &[]).unwrap(),
                Value::String(text.to_uppercase())
            );
            assert_eq!(
                lower(&args, &[]).unwrap(),
                Value::String(text.to_lowercase())
            );
        }
    }
}
