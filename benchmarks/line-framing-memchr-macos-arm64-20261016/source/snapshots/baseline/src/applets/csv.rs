//! Finite, string-preserving CSV conversion using the common RFC 4180 format.
//!
//! Parsing never guesses field types. A table is represented as a header list
//! and a rectangular list of string rows so duplicate names cannot overwrite
//! fields. The parser accepts RFC 4180 CRLF records and LF records used by
//! Unix tools; the stringifier always emits CRLF records.

use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use std::collections::HashSet;

const MAX_CSV_BYTES: usize = 16 * 1024 * 1024;
const MAX_CSV_ROWS: usize = 100_000;
const MAX_CSV_COLUMNS: usize = 16_384;
const MAX_CSV_CELLS: usize = 1_000_000;

pub(super) fn parse(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = match args.get("text") {
        Some(Value::String(text)) => text.as_bytes(),
        Some(_) => return Err(invalid("args.text must be a string")),
        None => input,
    };
    if text.len() > MAX_CSV_BYTES {
        return Err(invalid(format!(
            "CSV input exceeds the {MAX_CSV_BYTES}-byte limit"
        )));
    }
    std::str::from_utf8(text)
        .map_err(|error| invalid(format!("CSV input is not valid UTF-8: {error}")))?;
    context.checkpoint()?;
    let has_headers = match args.get("has_headers") {
        None => true,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("args.has_headers must be a boolean")),
    };

    let mut records = read_records(text, has_headers, context)?;
    let headers = if has_headers && !records.is_empty() {
        records.remove(0)
    } else {
        Vec::new()
    };
    validate_table(&headers, &records, context)?;

    let value = Value::Object(Map::from_iter([
        (
            "headers".to_owned(),
            Value::Array(headers.into_iter().map(Value::String).collect()),
        ),
        (
            "rows".to_owned(),
            Value::Array(
                records
                    .into_iter()
                    .map(|row| Value::Array(row.into_iter().map(Value::String).collect()))
                    .collect(),
            ),
        ),
    ]));
    check_json_size(&value, context, "parsed CSV table")?;
    Ok(value)
}

pub(super) fn stringify(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let table = args
        .get("table")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("args.table must be an object with headers and rows"))?;
    if table.len() != 2 || !table.contains_key("headers") || !table.contains_key("rows") {
        return Err(invalid(
            "args.table must contain exactly the headers and rows fields",
        ));
    }
    let header_values = table["headers"]
        .as_array()
        .ok_or_else(|| invalid("args.table.headers must be an array of strings"))?;
    if header_values.len() > MAX_CSV_COLUMNS {
        return Err(invalid(format!(
            "CSV table exceeds the {MAX_CSV_COLUMNS}-column limit"
        )));
    }
    let mut source_bytes = 0usize;
    let mut source_cells = 0usize;
    let mut headers = Vec::with_capacity(header_values.len());
    for (index, value) in header_values.iter().enumerate() {
        checkpoint_work(context, source_cells)?;
        let header = value
            .as_str()
            .ok_or_else(|| invalid(format!("args.table.headers[{index}] must be a string")))?;
        source_bytes = checked_source_bytes(source_bytes, header.len())?;
        source_cells = checked_cells(source_cells, 1)?;
        headers.push(header);
    }
    let row_values = table["rows"]
        .as_array()
        .ok_or_else(|| invalid("args.table.rows must be an array of string arrays"))?;
    if row_values.len() > MAX_CSV_ROWS {
        return Err(invalid(format!(
            "CSV table exceeds the {MAX_CSV_ROWS}-row limit"
        )));
    }
    let mut rows = Vec::with_capacity(row_values.len());
    for (row_index, row_value) in row_values.iter().enumerate() {
        checkpoint_work(context, source_cells)?;
        let row_values = row_value.as_array().ok_or_else(|| {
            invalid(format!(
                "args.table.rows[{row_index}] must be an array of strings"
            ))
        })?;
        if row_values.len() > MAX_CSV_COLUMNS {
            return Err(invalid(format!(
                "CSV row {row_index} exceeds the {MAX_CSV_COLUMNS}-column limit"
            )));
        }
        let mut row = Vec::with_capacity(row_values.len());
        for (column_index, value) in row_values.iter().enumerate() {
            checkpoint_work(context, source_cells)?;
            let cell = value.as_str().ok_or_else(|| {
                invalid(format!(
                    "args.table.rows[{row_index}][{column_index}] must be a string"
                ))
            })?;
            source_bytes = checked_source_bytes(source_bytes, cell.len())?;
            source_cells = checked_cells(source_cells, 1)?;
            row.push(cell);
        }
        rows.push(row);
    }
    validate_table(&headers, &rows, context)?;

    let mut output = String::new();
    let mut cells_written = 0usize;
    let mut work = 0usize;
    if !headers.is_empty() {
        write_record(
            &headers,
            &mut output,
            &mut cells_written,
            &mut work,
            context,
        )?;
    }
    for row in &rows {
        checkpoint_work(context, work)?;
        work = work.saturating_add(1);
        write_record(row, &mut output, &mut cells_written, &mut work, context)?;
    }
    let value = Value::String(output);
    check_json_size(&value, context, "serialized CSV text")?;
    Ok(value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldState {
    StartOfRecord,
    StartOfField,
    Unquoted,
    Quoted,
    AfterQuote,
}

fn read_records(
    text: &[u8],
    has_headers: bool,
    context: &ExecutionContext<'_>,
) -> Result<Vec<Vec<String>>, AppletError> {
    // The table limit counts data rows. A header consumes a CSV record but
    // does not consume one of those rows.
    let max_records = if has_headers {
        MAX_CSV_ROWS + 1
    } else {
        MAX_CSV_ROWS
    };
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = Vec::new();
    let mut state = FieldState::StartOfRecord;
    let mut total_cells = 0usize;
    let mut index = 0usize;
    let mut work = 0usize;

    while index < text.len() {
        checkpoint_work(context, work)?;
        work = work.saturating_add(1);
        let byte = text[index];
        match state {
            FieldState::StartOfRecord | FieldState::StartOfField => match byte {
                b'"' => state = FieldState::Quoted,
                b',' => {
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    state = FieldState::StartOfField;
                }
                b'\n' => {
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    push_record(&mut records, &mut record, max_records)?;
                    state = FieldState::StartOfRecord;
                }
                b'\r' => {
                    if text.get(index + 1) != Some(&b'\n') {
                        return Err(invalid(format!(
                            "bare carriage return outside a quoted field at byte {index}"
                        )));
                    }
                    checkpoint_work(context, work)?;
                    work = work.saturating_add(1);
                    index += 1;
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    push_record(&mut records, &mut record, max_records)?;
                    state = FieldState::StartOfRecord;
                }
                _ => {
                    validate_data_byte(byte, index)?;
                    field.push(byte);
                    state = FieldState::Unquoted;
                }
            },
            FieldState::Unquoted => match byte {
                b',' => {
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    state = FieldState::StartOfField;
                }
                b'\n' => {
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    push_record(&mut records, &mut record, max_records)?;
                    state = FieldState::StartOfRecord;
                }
                b'\r' => {
                    if text.get(index + 1) != Some(&b'\n') {
                        return Err(invalid(format!(
                            "bare carriage return outside a quoted field at byte {index}"
                        )));
                    }
                    checkpoint_work(context, work)?;
                    work = work.saturating_add(1);
                    index += 1;
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    push_record(&mut records, &mut record, max_records)?;
                    state = FieldState::StartOfRecord;
                }
                b'"' => {
                    return Err(invalid(format!(
                        "quote in an unquoted CSV field at byte {index}"
                    )));
                }
                _ => {
                    validate_data_byte(byte, index)?;
                    field.push(byte);
                }
            },
            FieldState::Quoted => match byte {
                b'"' if text.get(index + 1) == Some(&b'"') => {
                    checkpoint_work(context, work)?;
                    work = work.saturating_add(1);
                    index += 1;
                    field.push(b'"');
                }
                b'"' => state = FieldState::AfterQuote,
                _ => {
                    if byte != b'\r' && byte != b'\n' {
                        validate_data_byte(byte, index)?;
                    }
                    field.push(byte);
                }
            },
            FieldState::AfterQuote => match byte {
                b',' => {
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    state = FieldState::StartOfField;
                }
                b'\n' => {
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    push_record(&mut records, &mut record, max_records)?;
                    state = FieldState::StartOfRecord;
                }
                b'\r' => {
                    if text.get(index + 1) != Some(&b'\n') {
                        return Err(invalid(format!(
                            "bare carriage return outside a quoted field at byte {index}"
                        )));
                    }
                    checkpoint_work(context, work)?;
                    work = work.saturating_add(1);
                    index += 1;
                    push_field(&mut record, &mut field)?;
                    total_cells = checked_cells(total_cells, 1)?;
                    push_record(&mut records, &mut record, max_records)?;
                    state = FieldState::StartOfRecord;
                }
                _ => {
                    return Err(invalid(format!(
                        "unexpected byte after closing CSV quote at byte {index}"
                    )));
                }
            },
        }
        index += 1;
    }

    match state {
        FieldState::Quoted => return Err(invalid("CSV input ends inside a quoted field")),
        FieldState::StartOfRecord => {}
        FieldState::StartOfField | FieldState::Unquoted | FieldState::AfterQuote => {
            push_field(&mut record, &mut field)?;
            total_cells = checked_cells(total_cells, 1)?;
            push_record(&mut records, &mut record, max_records)?;
        }
    }
    context.checkpoint()?;
    if total_cells > MAX_CSV_CELLS {
        return Err(invalid(format!(
            "CSV input exceeds the {MAX_CSV_CELLS}-cell limit"
        )));
    }
    Ok(records)
}

fn push_field(record: &mut Vec<String>, field: &mut Vec<u8>) -> Result<(), AppletError> {
    if record.len() >= MAX_CSV_COLUMNS {
        return Err(invalid(format!(
            "CSV record exceeds the {MAX_CSV_COLUMNS}-column limit"
        )));
    }
    let value = String::from_utf8(std::mem::take(field))
        .map_err(|error| invalid(format!("CSV field is not valid UTF-8: {error}")))?;
    record.push(value);
    Ok(())
}

fn push_record(
    records: &mut Vec<Vec<String>>,
    record: &mut Vec<String>,
    max_records: usize,
) -> Result<(), AppletError> {
    if records.len() >= max_records {
        return Err(invalid(format!(
            "CSV input exceeds the {MAX_CSV_ROWS}-row table limit"
        )));
    }
    records.push(std::mem::take(record));
    Ok(())
}

fn checked_cells(total: usize, added: usize) -> Result<usize, AppletError> {
    let next = total
        .checked_add(added)
        .ok_or_else(|| invalid("CSV cell count overflowed"))?;
    if next > MAX_CSV_CELLS {
        return Err(invalid(format!(
            "CSV input exceeds the {MAX_CSV_CELLS}-cell limit"
        )));
    }
    Ok(next)
}

fn checked_source_bytes(total: usize, added: usize) -> Result<usize, AppletError> {
    let next = total
        .checked_add(added)
        .ok_or_else(|| invalid("CSV table byte count overflowed"))?;
    if next > MAX_CSV_BYTES {
        return Err(invalid(format!(
            "CSV table fields exceed the {MAX_CSV_BYTES}-byte limit"
        )));
    }
    Ok(next)
}

fn validate_data_byte(byte: u8, index: usize) -> Result<(), AppletError> {
    if byte < 0x20 || byte == 0x7f {
        return Err(invalid(format!(
            "unsupported control byte in CSV field at byte {index}"
        )));
    }
    Ok(())
}

fn validate_table<H, C>(
    headers: &[H],
    rows: &[Vec<C>],
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError>
where
    H: AsRef<str>,
    C: AsRef<str>,
{
    if headers.len() > MAX_CSV_COLUMNS {
        return Err(invalid(format!(
            "CSV table exceeds the {MAX_CSV_COLUMNS}-column limit"
        )));
    }
    if rows.len() > MAX_CSV_ROWS {
        return Err(invalid(format!(
            "CSV table exceeds the {MAX_CSV_ROWS}-row limit"
        )));
    }
    let mut seen = HashSet::with_capacity(headers.len());
    for (index, header) in headers.iter().enumerate() {
        checkpoint_work(context, index)?;
        if !seen.insert(header.as_ref()) {
            return Err(invalid(format!(
                "CSV header {:?} is duplicated at column {index}",
                header.as_ref()
            )));
        }
    }
    let expected_columns = if headers.is_empty() {
        rows.first().map_or(0, Vec::len)
    } else {
        headers.len()
    };
    let mut cell_count = headers.len();
    for (row_index, row) in rows.iter().enumerate() {
        checkpoint_work(context, row_index)?;
        if row.is_empty() {
            return Err(invalid(format!(
                "CSV row {row_index} must contain at least one field"
            )));
        }
        if row.len() > MAX_CSV_COLUMNS {
            return Err(invalid(format!(
                "CSV row {row_index} exceeds the {MAX_CSV_COLUMNS}-column limit"
            )));
        }
        if row.len() != expected_columns {
            return Err(invalid(format!(
                "CSV row {row_index} has {} fields; expected {expected_columns}",
                row.len()
            )));
        }
        cell_count = checked_cells(cell_count, row.len())?;
    }
    context.checkpoint()?;
    Ok(())
}

fn write_record<C: AsRef<str>>(
    row: &[C],
    output: &mut String,
    cells_written: &mut usize,
    work: &mut usize,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    for (index, field) in row.iter().enumerate() {
        checkpoint_work(context, *work)?;
        *work = work.saturating_add(1);
        if index > 0 {
            push_output(output, ",", work, context)?;
        }
        write_field(field.as_ref(), output, work, context)?;
        *cells_written = checked_cells(*cells_written, 1)?;
    }
    push_output(output, "\r\n", work, context)
}

fn write_field(
    field: &str,
    output: &mut String,
    work: &mut usize,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    let mut quoted = false;
    for character in field.chars() {
        checkpoint_work(context, *work)?;
        *work = work.saturating_add(1);
        quoted |= matches!(character, ',' | '"' | '\r' | '\n');
        if is_disallowed_control(character) {
            return Err(invalid(
                "CSV fields cannot contain unsupported control characters",
            ));
        }
    }
    if !quoted {
        for character in field.chars() {
            checkpoint_work(context, *work)?;
            *work = work.saturating_add(1);
            output.push(character);
            check_output_size(output)?;
        }
    } else {
        push_output(output, "\"", work, context)?;
        for character in field.chars() {
            checkpoint_work(context, *work)?;
            *work = work.saturating_add(1);
            if character == '"' {
                push_output(output, "\"\"", work, context)?;
            } else {
                output.push(character);
                check_output_size(output)?;
            }
        }
        push_output(output, "\"", work, context)?;
    }
    Ok(())
}

fn push_output(
    output: &mut String,
    value: &str,
    work: &mut usize,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    checkpoint_work(context, *work)?;
    *work = work.saturating_add(1);
    output.push_str(value);
    check_output_size(output)
}

fn check_output_size(output: &str) -> Result<(), AppletError> {
    if output.len() > MAX_CSV_BYTES {
        return Err(invalid(format!(
            "CSV output exceeds the {MAX_CSV_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn is_disallowed_control(character: char) -> bool {
    (character <= '\u{001f}' || character == '\u{007f}') && !matches!(character, '\r' | '\n')
}

fn check_json_size(
    value: &Value,
    context: &ExecutionContext<'_>,
    description: &str,
) -> Result<(), AppletError> {
    context.checkpoint()?;
    let encoded = serde_json::to_vec(value)
        .map_err(|error| invalid(format!("CSV table cannot be serialized as JSON: {error}")))?;
    if encoded.len().saturating_add(1) > MAX_CSV_BYTES {
        return Err(invalid(format!(
            "{description} and its output newline exceed the {MAX_CSV_BYTES}-byte JSON limit"
        )));
    }
    context.checkpoint()?;
    Ok(())
}
