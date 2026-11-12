use flow::{manifest::Registry, parser, runtime};
use std::{fs, path::Path, time::Duration};

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(cwd.into())
    }
}

/// Takes `/n` from `json` as `kind`, then encodes the binding. Returns the encoded bytes,
/// or the runtime error text when the typed take refuses the value.
fn take_outcome(kind: &str, json: &str) -> Result<Vec<u8>, String> {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.json"), json).unwrap();
    let source = format!(
        "read \"input.json\"\nthen parse json as doc\ntake {kind} at \"/n\" from doc as n\nthen encode n as json\n"
    );
    let program = parser::parse(&source).unwrap();
    runtime::run(&program, &Registry::default(), &options(dir.path()))
        .map(|result| result.output)
        .map_err(|error| format!("{error:#}"))
}

#[test]
fn unsigned_integer_take_accepts_nonnegative_whole_numbers() {
    assert_eq!(
        take_outcome("unsigned integer", r#"{"n": 0}"#).unwrap(),
        b"0"
    );
    assert_eq!(
        take_outcome("unsigned integer", r#"{"n": 4294967296}"#).unwrap(),
        b"4294967296"
    );
}

#[test]
fn unsigned_integer_take_refuses_signed_fractional_and_text_values() {
    for json in [
        r#"{"n": -1}"#,
        r#"{"n": 1.5}"#,
        r#"{"n": "7"}"#,
        r#"{"n": null}"#,
    ] {
        assert!(
            take_outcome("unsigned integer", json).is_err(),
            "unsigned integer accepted {json}"
        );
    }
}

#[test]
fn usize_take_accepts_nonnegative_whole_numbers_and_refuses_the_rest() {
    assert_eq!(take_outcome("usize", r#"{"n": 3}"#).unwrap(), b"3");
    for json in [r#"{"n": -1}"#, r#"{"n": 2.25}"#, r#"{"n": true}"#] {
        assert!(
            take_outcome("usize", json).is_err(),
            "usize accepted {json}"
        );
    }
}

#[test]
fn integer_take_still_refuses_fractional_values() {
    assert_eq!(take_outcome("integer", r#"{"n": -4}"#).unwrap(), b"-4");
    assert!(take_outcome("integer", r#"{"n": 1.5}"#).is_err());
}

#[test]
fn incomplete_numeric_type_names_are_parse_errors() {
    for source in [
        "take unsigned at \"/n\" from doc as n",
        "take unsigned text at \"/n\" from doc as n",
    ] {
        assert!(parser::parse(source).is_err(), "parsed {source:?}");
    }
    assert!(parser::parse("take unsigned integer at \"/n\" from doc as n").is_ok());
    assert!(parser::parse("take usize at \"/n\" from doc as n").is_ok());
}
