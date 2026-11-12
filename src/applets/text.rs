use super::list::usize_within;
use super::{
    checkpoint_work, invalid, io_error, optional_string, required_string, AppletError,
    ExecutionContext,
};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use unicode_width::UnicodeWidthStr;

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
                .and_then(|width| usize::try_from(width).ok())
                .ok_or_else(|| invalid("args.width must be a nonnegative integer in range"))?;
            let ellipsis = optional_string(args, "ellipsis", "…")?;
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
                .ok_or_else(|| invalid("args.length must be a nonnegative integer"))?;
            let length = usize_within(length, usize::MAX as u64)
                .ok_or_else(|| invalid("substring length is out of range"))?;
            if args.get("unit").and_then(Value::as_str) == Some("bytes") {
                let start = usize_within(start.unsigned_abs(), usize::MAX as u64)
                    .and_then(|magnitude| {
                        if start < 0 {
                            text.len().checked_sub(magnitude)
                        } else {
                            Some(magnitude)
                        }
                    })
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
            let start = usize_within(start.unsigned_abs(), usize::MAX as u64)
                .and_then(|magnitude| {
                    if start < 0 {
                        chars.len().checked_sub(magnitude)
                    } else {
                        Some(magnitude)
                    }
                })
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
            let display_width = args
                .get("display_width")
                .map(|value| {
                    value
                        .as_bool()
                        .ok_or_else(|| invalid("args.display_width must be a boolean"))
                })
                .transpose()?
                .unwrap_or(false);
            let width = args
                .get("width")
                .and_then(Value::as_u64)
                .and_then(|width| usize::try_from(width).ok())
                .ok_or_else(|| invalid("args.width must be a nonnegative integer in range"))?;
            let side = optional_string(args, "side", "right")?;
            let fill = optional_string(args, "fill", " ")?;
            if fill.is_empty() {
                return Err(invalid("args.fill cannot be empty"));
            }
            if display_width {
                let padding_width = width.saturating_sub(UnicodeWidthStr::width(text));
                let fill_width = UnicodeWidthStr::width(fill);
                if padding_width > 0 && fill_width == 0 {
                    return Err(invalid("args.fill must have positive display width"));
                }
                let mut padding = String::new();
                if let Some(full_fills) = padding_width.checked_div(fill_width) {
                    let remainder = padding_width % fill_width;
                    if full_fills
                        .checked_mul(fill.len())
                        .and_then(|bytes| bytes.checked_add(remainder))
                        .and_then(|bytes| bytes.checked_add(text.len()))
                        .is_none_or(|bytes| bytes > MAX_TEXT_BYTES)
                    {
                        return Err(invalid("text.pad output exceeds the text limit"));
                    }
                    for _ in 0..full_fills {
                        padding.push_str(fill);
                    }
                    padding.extend(std::iter::repeat_n(' ', remainder));
                }
                let join = |padding: &str| -> Result<String, AppletError> {
                    match side {
                        "left" => Ok(format!("{padding}{text}")),
                        "right" => Ok(format!("{text}{padding}")),
                        _ => Err(invalid("side must be left or right")),
                    }
                };
                // A joiner or combining mark at the seam can merge the fill with
                // the text, so the joined width may fall short of the estimate.
                // Keep adding fill while each addition makes progress.
                let mut value = join(&padding)?;
                let mut joined_width = UnicodeWidthStr::width(value.as_str());
                while joined_width < width {
                    padding.push_str(fill);
                    check_text_size(text.len() + padding.len())?;
                    value = join(&padding)?;
                    let next_width = UnicodeWidthStr::width(value.as_str());
                    if next_width <= joined_width {
                        return Err(invalid(
                            "args.fill cannot reach the requested display width when joined to text; use a fill whose characters add display width",
                        ));
                    }
                    joined_width = next_width;
                }
                return text_output(value);
            }
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
                .and_then(|count| usize::try_from(count).ok())
                .ok_or_else(|| invalid("args.count must be a nonnegative integer in range"))?;
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
    let text = text_input(args, input)?;
    if text.is_ascii() {
        text_output(crate::bytes::ascii_case_string(text, false))
    } else {
        text_output(text.to_lowercase())
    }
}

/// Unicode default uppercase mapping. It is locale-independent.
/// ASCII input uses the shared byte-kernel case map, which is identical to
/// [`str::to_uppercase`] over the ASCII range; every non-ASCII
/// string keeps the Unicode mapping (including expanding mappings).
pub(super) fn upper(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    if text.is_ascii() {
        text_output(crate::bytes::ascii_case_string(text, true))
    } else {
        text_output(text.to_uppercase())
    }
}

/// The longest UTF-8 character is four bytes, so an unfinished tail is at most three.
const UTF8_CARRY_BYTES: usize = 3;

/// Streaming form of [`upper`]. The input is read in pieces of at most
/// `chunk_size` bytes; a character cut at a piece boundary is carried into the
/// next read, so every character is mapped whole. Uppercase mapping is decided
/// one character at a time, so the concatenated output equals [`upper`] over
/// the whole input. Errors match the buffered path: invalid UTF-8 reports its
/// absolute offset, a character truncated at end of input is reported at its
/// start, and output over the byte limit is refused after the input is read to
/// the end. Unlike the buffered path, output from earlier pieces may already be
/// in `output` when an error is returned.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn upper_stream(
    args: &Map<String, Value>,
    input: &mut dyn Read,
    output: &mut dyn Write,
    context: &ExecutionContext<'_>,
    chunk_size: usize,
) -> Result<(), AppletError> {
    if args.contains_key("text") {
        // text_input takes args.text in preference to the stream, so the stream is not read.
        let text = text_input(args, &[])?;
        let mut mapped = Vec::with_capacity(text.len());
        push_upper(text, &mut mapped);
        check_text_size(mapped.len())?;
        return write_output(output, &mapped);
    }
    let chunk_size = chunk_size.max(1);
    // buf[..carry] holds an unfinished character; the next read fills the rest.
    let mut buf = vec![0u8; chunk_size + UTF8_CARRY_BYTES];
    let mut carry = 0usize;
    // Input offset of buf[0], used for error positions.
    let mut consumed = 0usize;
    let mut produced = 0usize;
    let mut mapped = Vec::new();
    loop {
        context.checkpoint()?;
        let read = match input.read(&mut buf[carry..carry + chunk_size]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(error, "reading applet input")),
        };
        let filled = carry + read;
        let (text, tail) = split_utf8(&buf[..filled], consumed)?;
        let tail_len = tail.len();
        mapped.clear();
        push_upper(text, &mut mapped);
        emit(output, &mapped, &mut produced)?;
        consumed += filled - tail_len;
        buf.copy_within(filled - tail_len..filled, 0);
        carry = tail_len;
    }
    if carry > 0 {
        return Err(utf8_error(consumed, None));
    }
    check_text_size(produced)
}

/// Append the uppercase UTF-8 of `text`. ASCII maps bytewise, exactly as
/// [`str::to_ascii_uppercase`] does, and other characters use their Unicode
/// mapping, which may change the byte length.
#[cfg_attr(not(test), allow(dead_code))]
fn push_upper(text: &str, out: &mut Vec<u8>) {
    let mut encoded = [0u8; 4];
    for character in text.chars() {
        if character.is_ascii() {
            out.push(character.to_ascii_uppercase() as u8);
        } else {
            for mapped in character.to_uppercase() {
                out.extend_from_slice(mapped.encode_utf8(&mut encoded).as_bytes());
            }
        }
    }
}

/// Split `bytes`, which start at input offset `start`, into the text before
/// any unfinished character and the unfinished tail (at most three bytes).
/// Invalid UTF-8 is an error at its absolute offset.
#[cfg_attr(not(test), allow(dead_code))]
fn split_utf8(bytes: &[u8], start: usize) -> Result<(&str, &[u8]), AppletError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok((text, &[])),
        Err(error) => {
            let valid = error.valid_up_to();
            if let Some(length) = error.error_len() {
                return Err(utf8_error(start + valid, Some(length)));
            }
            let text = std::str::from_utf8(&bytes[..valid])
                .map_err(|error| utf8_error(start + error.valid_up_to(), error.error_len()))?;
            Ok((text, &bytes[valid..]))
        }
    }
}

/// The message `text_input` produces for the same failure in the buffered path.
#[cfg_attr(not(test), allow(dead_code))]
fn utf8_error(index: usize, error_len: Option<usize>) -> AppletError {
    let detail = match error_len {
        Some(length) => format!("invalid utf-8 sequence of {length} bytes from index {index}"),
        None => format!("incomplete utf-8 byte sequence from index {index}"),
    };
    invalid(format!("stdin must be UTF-8: {detail}"))
}

/// Write `bytes` to `produced`'s budget. Output beyond the byte limit is not
/// written, and `produced` keeps counting so the final size check refuses it.
#[cfg_attr(not(test), allow(dead_code))]
fn emit(output: &mut dyn Write, bytes: &[u8], produced: &mut usize) -> Result<(), AppletError> {
    let room = super::shared::MAX_BYTES.saturating_sub(*produced);
    write_output(output, &bytes[..bytes.len().min(room)])?;
    *produced = produced.saturating_add(bytes.len());
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_output(output: &mut dyn Write, bytes: &[u8]) -> Result<(), AppletError> {
    output
        .write_all(bytes)
        .map_err(|error| io_error(error, "writing applet output"))
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
    use std::io::{Cursor, Read};

    #[test]
    fn substring_indices_past_usize_are_out_of_range_not_wrapped() {
        let context = ExecutionContext::unrestricted();
        let substring = |start: i64, length: u64, unit: Option<&str>| {
            let mut args = Map::from_iter([
                ("text".to_owned(), Value::String("héllo".to_owned())),
                ("start".to_owned(), Value::from(start)),
                ("length".to_owned(), Value::from(length)),
            ]);
            if let Some(unit) = unit {
                args.insert("unit".to_owned(), Value::String(unit.to_owned()));
            }
            shape("text.substring", &args, &[], &context)
        };
        assert!(substring(4_294_967_296, 1, None).is_err());
        assert!(substring(-4_294_967_296, 1, None).is_err());
        assert!(substring(0, 4_294_967_296, None).is_err());
        assert!(substring(4_294_967_296, 1, Some("bytes")).is_err());
        assert!(substring(-4_294_967_296, 1, Some("bytes")).is_err());
        assert!(substring(0, 4_294_967_296, Some("bytes")).is_err());
    }

    #[test]
    fn substring_boundary_cases_keep_unicode_and_byte_behavior() {
        let context = ExecutionContext::unrestricted();
        let substring = |start: i64, length: u64, unit: Option<&str>| {
            let mut args = Map::from_iter([
                ("text".to_owned(), Value::String("héllo".to_owned())),
                ("start".to_owned(), Value::from(start)),
                ("length".to_owned(), Value::from(length)),
            ]);
            if let Some(unit) = unit {
                args.insert("unit".to_owned(), Value::String(unit.to_owned()));
            }
            shape("text.substring", &args, &[], &context)
        };
        let text = |value: &str| Value::String(value.to_owned());
        assert_eq!(substring(1, 3, None).unwrap(), text("éll"));
        assert_eq!(substring(-2, 2, None).unwrap(), text("lo"));
        assert_eq!(substring(-5, 5, None).unwrap(), text("héllo"));
        assert_eq!(substring(5, 0, None).unwrap(), text(""));
        assert!(substring(-6, 0, None).is_err());
        assert_eq!(substring(0, 1, Some("bytes")).unwrap(), text("h"));
        assert_eq!(substring(1, 2, Some("bytes")).unwrap(), text("é"));
        assert_eq!(substring(-1, 1, Some("bytes")).unwrap(), text("o"));
        assert_eq!(substring(-6, 6, Some("bytes")).unwrap(), text("héllo"));
        assert!(substring(2, 1, Some("bytes")).is_err());
    }

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

    #[test]
    fn upper_ascii_fast_path_matches_unicode_mapping() {
        // Empty, boundary-length, and all-ASCII-byte inputs stay on the
        // ASCII path yet match the Unicode mapping.
        let all_ascii: String = (0u8..128).map(|byte| byte as char).collect();
        let mut cases = vec!["".to_owned(), "a".to_owned(), all_ascii];
        for len in [1, 2, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
            cases.push("aBcDeF 123 zZ!".repeat(len));
        }
        // Deterministic pseudo-random ASCII.
        let mut seed = 0x1234_5678u32;
        let mut random = String::new();
        for _ in 0..1024 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            random.push((seed % 128) as u8 as char);
        }
        cases.push(random);
        for text in &cases {
            assert!(text.is_ascii(), "case must stay on the ASCII path");
            let args = Map::from_iter([("text".to_owned(), Value::String(text.clone()))]);
            assert_eq!(
                upper(&args, &[]).unwrap(),
                Value::String(text.to_uppercase()),
                "text={text:?}"
            );
        }
        // Non-ASCII input keeps the Unicode mapping, including mappings
        // that expand to multiple characters.
        for text in ["ß", "ﬀ", "ﬁ", "ŉ", "Straße", "éclair ıß", "İΣῳ"] {
            assert!(!text.is_ascii());
            let args = Map::from_iter([("text".to_owned(), Value::String(text.to_owned()))]);
            assert_eq!(
                upper(&args, &[]).unwrap(),
                Value::String(text.to_uppercase()),
                "text={text:?}"
            );
        }
        assert_eq!("ß".to_uppercase(), "SS");
    }

    #[test]
    fn display_width_pad_never_returns_short_output_across_joiners() {
        let context = ExecutionContext::unrestricted();
        let pad = |text: &str, side: &str, fill: &str| {
            let args = Map::from_iter([
                ("text".to_owned(), Value::String(text.to_owned())),
                ("width".to_owned(), Value::from(4)),
                ("side".to_owned(), Value::String(side.to_owned())),
                ("fill".to_owned(), Value::String(fill.to_owned())),
                ("display_width".to_owned(), Value::Bool(true)),
            ]);
            shape("text.pad", &args, &[], &context)
        };
        // Each case must either reach width 4 or be rejected with an error.
        for side in ["right", "left"] {
            for text in ["\u{1F469}", "e", "e\u{301}"] {
                for fill in [
                    "\u{200D}\u{1F4BB}",
                    "a\u{200D}\u{1F4BB}",
                    "\u{301}a",
                    "e\u{301}",
                ] {
                    match pad(text, side, fill) {
                        Err(_) => {}
                        Ok(Value::String(value)) => assert!(
                            UnicodeWidthStr::width(value.as_str()) >= 4,
                            "side={side:?}, text={text:?}, fill={fill:?}, value={value:?}"
                        ),
                        Ok(other) => panic!("unexpected value {other:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn display_width_pad_keeps_normal_fill_behavior() {
        let context = ExecutionContext::unrestricted();
        let pad = |text: &str, width: u64, side: &str, fill: &str| {
            let args = Map::from_iter([
                ("text".to_owned(), Value::String(text.to_owned())),
                ("width".to_owned(), Value::from(width)),
                ("side".to_owned(), Value::String(side.to_owned())),
                ("fill".to_owned(), Value::String(fill.to_owned())),
                ("display_width".to_owned(), Value::Bool(true)),
            ]);
            shape("text.pad", &args, &[], &context).unwrap()
        };
        let text = |value: &str| Value::String(value.to_owned());
        assert_eq!(pad("ab", 6, "right", "xy"), text("abxyxy"));
        assert_eq!(pad("a", 4, "right", "\u{4E2D}"), text("a\u{4E2D} "));
        assert_eq!(pad("a", 4, "left", "\u{4E2D}"), text("\u{4E2D} a"));
        assert_eq!(
            pad("\u{1F469}", 4, "right", "\u{1F4BB}"),
            text("\u{1F469}\u{1F4BB}")
        );
    }

    /// Valid UTF-8 of exactly `length` bytes built from 1-, 2-, 3- and 4-byte
    /// characters, including characters whose uppercase has another length.
    fn mixed_utf8(length: usize) -> Vec<u8> {
        const PIECES: [&str; 7] = ["a", "é", "ﬃ", "𐐨", "€", "😀", "ŉ"];
        let mut bytes = Vec::with_capacity(length);
        let mut next = 0;
        while bytes.len() < length {
            let piece = PIECES[next % PIECES.len()];
            next += 1;
            let piece = if piece.len() <= length - bytes.len() {
                piece
            } else {
                "a"
            };
            bytes.extend_from_slice(piece.as_bytes());
        }
        bytes
    }

    fn buffered_upper(input: &[u8]) -> Result<Vec<u8>, (&'static str, String)> {
        match upper(&Map::new(), input) {
            Ok(Value::String(text)) => Ok(text.into_bytes()),
            Ok(other) => panic!("upper returned {other:?}"),
            Err(error) => Err((error.code, error.message)),
        }
    }

    /// Returns at most one byte per read, whatever the buffer size.
    struct OneByte<'a>(&'a [u8]);

    impl Read for OneByte<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.split_first() {
                Some((&first, rest)) if !buf.is_empty() => {
                    buf[0] = first;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    fn streamed_upper(
        input: &[u8],
        chunk_size: usize,
        one_byte: bool,
    ) -> (Result<(), (&'static str, String)>, Vec<u8>) {
        let mut sink = Vec::new();
        let context = ExecutionContext::unrestricted();
        let result = if one_byte {
            upper_stream(
                &Map::new(),
                &mut OneByte(input),
                &mut sink,
                &context,
                chunk_size,
            )
        } else {
            upper_stream(
                &Map::new(),
                &mut Cursor::new(input),
                &mut sink,
                &context,
                chunk_size,
            )
        };
        (result.map_err(|error| (error.code, error.message)), sink)
    }

    fn assert_stream_matches_buffered(input: &[u8]) {
        let expected = buffered_upper(input);
        for chunk_size in 1..=9 {
            for one_byte in [false, true] {
                let (result, sink) = streamed_upper(input, chunk_size, one_byte);
                match (&expected, &result) {
                    (Ok(bytes), Ok(())) => {
                        assert_eq!(&sink, bytes, "input {input:?}, chunk {chunk_size}")
                    }
                    (Err(expected_error), Err(actual_error)) => {
                        assert_eq!(
                            actual_error, expected_error,
                            "input {input:?}, chunk {chunk_size}"
                        );
                        // Output written before the error must be a prefix of the
                        // output for the valid text that precedes the bad byte.
                        let valid = match std::str::from_utf8(input) {
                            Ok(_) => input.len(),
                            Err(error) => error.valid_up_to(),
                        };
                        let prefix = buffered_upper(&input[..valid]).expect("valid prefix");
                        assert!(
                            prefix.starts_with(&sink),
                            "input {input:?}, chunk {chunk_size}"
                        );
                    }
                    _ => panic!(
                        "streaming and buffered disagree for {input:?}, chunk {chunk_size}: \
                         buffered {expected:?}, streamed {result:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn streaming_upper_matches_buffered_for_every_length_and_chunk_size() {
        for length in 0..=70 {
            let valid = mixed_utf8(length);
            let middle = length / 2;
            let mut stray_continuation = valid.clone();
            stray_continuation.insert(middle, 0x80);
            let mut invalid_byte = valid.clone();
            invalid_byte.insert(middle, 0xFF);
            let mut truncated_four = valid.clone();
            truncated_four.extend_from_slice(&[0xF0, 0x9F, 0x98]);
            let mut truncated_three = valid.clone();
            truncated_three.push(0xE2);
            let mut bad_continuation = valid.clone();
            bad_continuation.extend_from_slice(&[0xC3, 0x28]);
            for input in [
                valid,
                stray_continuation,
                invalid_byte,
                truncated_four,
                truncated_three,
                bad_continuation,
            ] {
                assert_stream_matches_buffered(&input);
            }
        }
    }

    #[test]
    fn streaming_upper_refuses_output_over_the_limit_like_the_buffered_path() {
        let input = vec![b'a'; crate::applets::shared::MAX_BYTES + 1];
        let expected = buffered_upper(&input).expect_err("buffered output over limit");
        let (result, sink) = streamed_upper(&input, 64 * 1024, false);
        assert_eq!(result.expect_err("streamed output over limit"), expected);
        assert_eq!(sink.len(), crate::applets::shared::MAX_BYTES);
        assert!(sink.iter().all(|&byte| byte == b'A'));
    }

    #[test]
    fn streaming_upper_observes_cancellation_before_reading() {
        let checkpoint = || -> Result<(), crate::applets::ExecutionBudgetExceeded> {
            Err(crate::applets::ExecutionBudgetExceeded::Cancelled)
        };
        let context = ExecutionContext::new(&checkpoint);
        let mut sink = Vec::new();
        let error = upper_stream(
            &Map::new(),
            &mut Cursor::new(b"abc".as_slice()),
            &mut sink,
            &context,
            2,
        )
        .expect_err("cancelled before the first read");
        assert_eq!(error.kind, crate::applets::AppletErrorKind::Cancelled);
        assert!(sink.is_empty());
    }

    #[test]
    fn streaming_upper_with_text_argument_does_not_read_the_stream() {
        let args = Map::from_iter([("text".to_owned(), Value::String("straße €".to_owned()))]);
        let mut sink = Vec::new();
        upper_stream(
            &args,
            &mut Cursor::new([0xFF_u8].as_slice()),
            &mut sink,
            &ExecutionContext::unrestricted(),
            4,
        )
        .expect("args.text takes precedence over the stream");
        assert_eq!(sink, "STRASSE €".as_bytes());
    }

    #[test]
    fn lower_is_not_chunk_safe_because_of_final_sigma() {
        // The final-sigma rule looks at the neighbours of a capital sigma, so a
        // piece boundary inside a word changes the result. Uppercase has no such
        // rule, which is why only upper is streamed.
        assert_eq!("AΣ".to_lowercase(), "aς");
        assert_eq!(
            format!("{}{}", "A".to_lowercase(), "Σ".to_lowercase()),
            "aσ"
        );
    }
}
