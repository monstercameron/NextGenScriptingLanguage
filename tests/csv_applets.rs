use flow::applets::{
    self, AppletError, AppletErrorKind, ExecutionBudgetExceeded, ExecutionContext,
};
use flow::contracts::{OutputEncoding, OutputType};
use flow::manifest::Registry;
use flow::{parser, runtime};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MAX_CSV_BYTES: usize = 16 * 1024 * 1024;
const MAX_CSV_ROWS: usize = 100_000;
const MAX_CSV_COLUMNS: usize = 16_384;
const MAX_CSV_CELLS: usize = 1_000_000;

fn run(operation: &str, args: Value, input: &[u8]) -> Result<Value, AppletError> {
    let args = args.as_object().cloned().unwrap_or_default();
    applets::execute(operation, &args, input)
}

fn registry() -> Registry {
    Registry::from_sources(&[
        include_str!("../manifests/core.csv_parse.json").to_owned(),
        include_str!("../manifests/core.csv_stringify.json").to_owned(),
    ])
    .unwrap()
}

fn single_header_table(header: String) -> Value {
    Value::Object(Map::from_iter([
        (
            "headers".to_owned(),
            Value::Array(vec![Value::String(header)]),
        ),
        ("rows".to_owned(), Value::Array(Vec::new())),
    ]))
}

#[test]
fn parser_preserves_unicode_quotes_crlf_multiline_and_text_values() {
    let parsed = run(
        "csv.parse",
        json!({}),
        "name,note,count,empty\r\n雪,\"say \"\"hi\"\"\r\nthere\",001,\r\n".as_bytes(),
    )
    .unwrap();

    assert_eq!(
        parsed,
        json!({
            "headers": ["name", "note", "count", "empty"],
            "rows": [["雪", "say \"hi\"\r\nthere", "001", ""]]
        })
    );

    let no_header = run(
        "csv.parse",
        json!({"has_headers": false}),
        "雪,001\n".as_bytes(),
    )
    .unwrap();
    assert_eq!(no_header, json!({"headers": [], "rows": [["雪", "001"]]}));
}

#[test]
fn empty_input_empty_cells_blank_records_and_final_newlines_are_distinct() {
    assert_eq!(
        run("csv.parse", json!({"has_headers": false}), b"").unwrap(),
        json!({"headers": [], "rows": []})
    );
    assert_eq!(
        run("csv.parse", json!({"has_headers": false}), b"\"\"").unwrap(),
        json!({"headers": [], "rows": [[""]]})
    );
    assert_eq!(
        run("csv.parse", json!({"has_headers": false}), b"\r\n").unwrap(),
        json!({"headers": [], "rows": [[""]]})
    );
    assert_eq!(
        run(
            "csv.parse",
            json!({"has_headers": false}),
            b"left,right\r\n"
        )
        .unwrap(),
        json!({"headers": [], "rows": [["left", "right"]]})
    );
    assert_eq!(
        run("csv.parse", json!({}), b"name\r\n").unwrap(),
        json!({"headers": ["name"], "rows": []})
    );
}

#[test]
fn bom_is_stripped_before_csv_parsing() {
    let parsed = run(
        "csv.parse",
        json!({}),
        "\u{feff}name,value\r\nAda,雪\r\n".as_bytes(),
    )
    .unwrap();
    assert_eq!(parsed["headers"][0], "name");
    assert_eq!(parsed["rows"][0][1], "雪");
}

#[test]
fn lf_records_canonicalize_to_crlf_while_unicode_and_multiline_bytes_roundtrip() {
    let input = "name,nfc,nfd,note\nAda,é,e\u{301},\"first\nsecond\r\nthird\"\n";
    let parsed = run("csv.parse", json!({}), input.as_bytes()).unwrap();
    let row = parsed["rows"][0].as_array().unwrap();
    assert_eq!(row[0], "Ada");
    assert_eq!(row[1].as_str().unwrap().as_bytes(), "é".as_bytes());
    assert_eq!(row[2].as_str().unwrap().as_bytes(), "e\u{301}".as_bytes());
    assert_eq!(row[3], "first\nsecond\r\nthird");

    let encoded = run("csv.stringify", json!({"table": parsed}), b"").unwrap();
    assert_eq!(
        encoded,
        json!("name,nfc,nfd,note\r\nAda,é,e\u{301},\"first\nsecond\r\nthird\"\r\n")
    );
    assert_eq!(
        run("csv.parse", json!({}), encoded.as_str().unwrap().as_bytes()).unwrap(),
        parsed
    );
}

#[test]
fn parser_enforces_exact_data_row_limit_with_and_without_headers() {
    let no_header_input = "x\n".repeat(MAX_CSV_ROWS);
    let no_header = run(
        "csv.parse",
        json!({"has_headers": false}),
        no_header_input.as_bytes(),
    )
    .unwrap();
    assert_eq!(no_header["rows"].as_array().unwrap().len(), MAX_CSV_ROWS);
    drop(no_header);
    drop(no_header_input);

    let too_many_no_header = "x\n".repeat(MAX_CSV_ROWS + 1);
    let error = run(
        "csv.parse",
        json!({"has_headers": false}),
        too_many_no_header.as_bytes(),
    )
    .unwrap_err();
    assert!(error.message.contains("100000-row table limit"));
    drop(too_many_no_header);

    let mut header_input = String::from("header\n");
    header_input.push_str(&"x\n".repeat(MAX_CSV_ROWS));
    let with_header = run("csv.parse", json!({}), header_input.as_bytes()).unwrap();
    assert_eq!(with_header["rows"].as_array().unwrap().len(), MAX_CSV_ROWS);
    drop(with_header);

    header_input.push_str("x\n");
    let error = run("csv.parse", json!({}), header_input.as_bytes()).unwrap_err();
    assert!(error.message.contains("100000-row table limit"));
}

#[test]
fn parser_and_stringifier_enforce_exact_column_limit() {
    let exact_columns = ",".repeat(MAX_CSV_COLUMNS - 1);
    let parsed = run(
        "csv.parse",
        json!({"has_headers": false}),
        exact_columns.as_bytes(),
    )
    .unwrap();
    assert_eq!(parsed["rows"][0].as_array().unwrap().len(), MAX_CSV_COLUMNS);

    let too_many_columns = ",".repeat(MAX_CSV_COLUMNS);
    let error = run(
        "csv.parse",
        json!({"has_headers": false}),
        too_many_columns.as_bytes(),
    )
    .unwrap_err();
    assert!(error.message.contains("16384-column limit"));

    let headers = |count: usize| {
        Value::Array(
            (0..count)
                .map(|index| Value::String(format!("column-{index}")))
                .collect(),
        )
    };
    let exact_table = json!({"headers": headers(MAX_CSV_COLUMNS), "rows": []});
    assert!(run("csv.stringify", json!({"table": exact_table}), b"").is_ok());
    let oversized_table = json!({"headers": headers(MAX_CSV_COLUMNS + 1), "rows": []});
    let error = run("csv.stringify", json!({"table": oversized_table}), b"").unwrap_err();
    assert!(error.message.contains("16384-column limit"));
}

#[test]
fn parser_and_stringifier_enforce_exact_cell_limit() {
    const WIDTH: usize = 1_000;
    const EXACT_ROWS: usize = MAX_CSV_CELLS / WIDTH;
    let exact_record = format!("{}\n", ",".repeat(WIDTH - 1));
    let exact_csv = exact_record.repeat(EXACT_ROWS);
    let parsed = run(
        "csv.parse",
        json!({"has_headers": false}),
        exact_csv.as_bytes(),
    )
    .unwrap();
    assert_eq!(parsed["rows"].as_array().unwrap().len(), EXACT_ROWS);
    assert_eq!(parsed["rows"][0].as_array().unwrap().len(), WIDTH);

    let too_many_csv = format!("{exact_csv}{exact_record}");
    let error = run(
        "csv.parse",
        json!({"has_headers": false}),
        too_many_csv.as_bytes(),
    )
    .unwrap_err();
    assert!(error.message.contains("1000000-cell limit"));
    drop(parsed);
    drop(exact_csv);
    drop(too_many_csv);

    let empty_row = Value::Array(vec![Value::String(String::new()); WIDTH]);
    let rows = vec![empty_row.clone(); EXACT_ROWS];
    let table = Value::Object(Map::from_iter([
        ("headers".to_owned(), Value::Array(Vec::new())),
        ("rows".to_owned(), Value::Array(rows)),
    ]));
    let args = Map::from_iter([("table".to_owned(), table)]);
    let output = applets::execute("csv.stringify", &args, b"").unwrap();
    assert_eq!(output.as_str().unwrap().len(), EXACT_ROWS * (3 * WIDTH + 1));
    drop(output);

    let mut oversized_args = args;
    oversized_args
        .get_mut("table")
        .unwrap()
        .get_mut("rows")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(empty_row);
    let error = applets::execute("csv.stringify", &oversized_args, b"").unwrap_err();
    assert!(error.message.contains("1000000-cell limit"));
}

#[test]
fn stringifier_enforces_exact_data_row_limit() {
    let rows = vec![json!(["x"]); MAX_CSV_ROWS];
    let args = Map::from_iter([("table".to_owned(), json!({"headers": ["h"], "rows": rows}))]);
    let output = applets::execute("csv.stringify", &args, b"").unwrap();
    assert_eq!(output.as_str().unwrap().len(), 3 * (MAX_CSV_ROWS + 1));

    let mut oversized_args = args;
    oversized_args
        .get_mut("table")
        .unwrap()
        .get_mut("rows")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(json!(["x"]));
    let error = applets::execute("csv.stringify", &oversized_args, b"").unwrap_err();
    assert!(error.message.contains("100000-row limit"));
}

#[test]
fn parser_distinguishes_csv_input_and_encoded_json_output_byte_limits() {
    {
        let at_input_limit = "x".repeat(MAX_CSV_BYTES);
        let args = Map::from_iter([("text".to_owned(), Value::String(at_input_limit))]);
        let error = applets::execute("csv.parse", &args, b"").unwrap_err();
        assert!(error.message.contains("parsed CSV table"));
        assert!(error.message.contains("JSON limit"));
    }

    {
        let over_input_limit = "x".repeat(MAX_CSV_BYTES + 1);
        let args = Map::from_iter([("text".to_owned(), Value::String(over_input_limit))]);
        let error = applets::execute("csv.parse", &args, b"").unwrap_err();
        assert!(error
            .message
            .contains("CSV input exceeds the 16777216-byte limit"));
    }

    {
        let at_field_limit = "x".repeat(MAX_CSV_BYTES);
        let args = Map::from_iter([("table".to_owned(), single_header_table(at_field_limit))]);
        let error = applets::execute("csv.stringify", &args, b"").unwrap_err();
        assert!(error
            .message
            .contains("CSV output exceeds the 16777216-byte limit"));
    }

    {
        let over_field_limit = "x".repeat(MAX_CSV_BYTES + 1);
        let args = Map::from_iter([("table".to_owned(), single_header_table(over_field_limit))]);
        let error = applets::execute("csv.stringify", &args, b"").unwrap_err();
        assert!(error
            .message
            .contains("CSV table fields exceed the 16777216-byte limit"));
    }
}

#[test]
fn parser_rejects_malformed_utf8_quotes_duplicate_headers_and_ragged_rows() {
    for input in [
        &b"a,b\r\n1\r\n"[..],
        &b"a\r\n\"unfinished"[..],
        &b"a\r\npre\"fix\r\n"[..],
        &b"a\r\n\"closed\"suffix\r\n"[..],
        &b"a\rb\r\n"[..],
        &b"a,a\r\n1,2\r\n"[..],
        &[0xff][..],
    ] {
        let error = run("csv.parse", json!({}), input).unwrap_err();
        assert_eq!(error.kind, AppletErrorKind::Permanent, "input: {input:?}");
    }
    let error = run(
        "csv.parse",
        json!({"has_headers": false}),
        b"one,two\r\nthree\r\n",
    )
    .unwrap_err();
    assert!(error.message.contains("expected 2"));
}

#[test]
fn stringify_is_deterministic_and_roundtrips_the_declared_table_shape() {
    let table = json!({
        "headers": ["name", "note", "amount", "empty"],
        "rows": [["雪", "say \"hi\"\r\nthere", "001", ""]]
    });
    let encoded = run("csv.stringify", json!({"table": table}), b"").unwrap();
    assert_eq!(
        encoded,
        json!("name,note,amount,empty\r\n雪,\"say \"\"hi\"\"\r\nthere\",001,\"\"\r\n")
    );
    let parsed = run("csv.parse", json!({}), encoded.as_str().unwrap().as_bytes()).unwrap();
    assert_eq!(
        parsed,
        json!({
            "headers": ["name", "note", "amount", "empty"],
            "rows": [["雪", "say \"hi\"\r\nthere", "001", ""]]
        })
    );
}

#[test]
fn stringify_rejects_numeric_coercion_and_nonrectangular_tables() {
    for table in [
        json!({"headers": ["count"], "rows": [[1]]}),
        json!({"headers": ["a", "b"], "rows": [["only one"]]}),
        json!({"headers": ["duplicate", "duplicate"], "rows": []}),
        json!({"headers": [], "rows": [[]]}),
        json!({"headers": ["a"], "rows": [], "extra": true}),
    ] {
        assert_eq!(
            run("csv.stringify", json!({"table": table}), b"")
                .unwrap_err()
                .kind,
            AppletErrorKind::Permanent
        );
    }
}

#[test]
fn registered_contracts_declare_recursive_table_shapes_and_compose() {
    let registry = registry();
    let parser_manifest = registry.resolve("csv.parse").unwrap();
    let parser_ports = parser_manifest.ports().unwrap();
    assert_eq!(
        parser_ports.outputs["value"].schema.kind,
        OutputType::Record
    );
    assert_eq!(
        parser_ports.outputs["value"].schema.fields["rows"]
            .items
            .as_deref()
            .unwrap()
            .kind,
        OutputType::List
    );
    assert_eq!(
        parser_ports.inputs["stdin"].encoding,
        Some(OutputEncoding::Raw)
    );
    let stringify_ports = registry.resolve("csv.stringify").unwrap().ports().unwrap();
    assert_eq!(
        stringify_ports.inputs["table"].schema.kind,
        OutputType::Record
    );
    assert_eq!(
        stringify_ports.inputs["table"].schema.fields["rows"]
            .items
            .as_deref()
            .unwrap()
            .kind,
        OutputType::List
    );

    let cwd = tempfile::tempdir().unwrap();
    let source = "run csv.parse with text \"name,count\\r\\nAda,001\" as table\nrun csv.stringify with table table as rendered\nthen encode rendered as json\n";
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &registry,
        &runtime::RunOptions {
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(5)),
            ..runtime::RunOptions::new(cwd.path().to_path_buf())
        },
    )
    .unwrap();
    assert_eq!(result.output, b"\"name,count\\r\\nAda,001\\r\\n\"");
}

#[test]
fn parser_observes_cooperative_cancellation_during_a_large_finite_input() {
    let text = "x".repeat(1024);
    let args = Map::from_iter([("text".to_owned(), Value::String(text))]);
    let checks = AtomicUsize::new(0);
    let checkpoint = || {
        if checks.fetch_add(1, Ordering::SeqCst) >= 3 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&checkpoint);
    let error = applets::execute_with_context("csv.parse", &args, b"", Path::new("."), &context)
        .unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Cancelled);
}

#[test]
fn stringifier_observes_cooperative_cancellation_inside_unicode_fields() {
    let args = Map::from_iter([(
        "table".to_owned(),
        json!({"headers": [], "rows": [["雪".repeat(1024)]]}),
    )]);
    let checks = AtomicUsize::new(0);
    let checkpoint = || {
        if checks.fetch_add(1, Ordering::SeqCst) >= 7 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&checkpoint);
    let error =
        applets::execute_with_context("csv.stringify", &args, b"", Path::new("."), &context)
            .unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Cancelled);
}
