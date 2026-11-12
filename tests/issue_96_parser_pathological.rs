//! Issue #96 red-team, dimension: language and parser, pathological input.
//!
//! Each input must come back from `flow::parser::parse` and `parse_collecting_errors`
//! within a time bound, without panicking. Either a program or at least one error must
//! be reported. Covered here: deep nesting, huge lines and files, unclosed raw blocks and
//! quotes, unbalanced brackets, NUL bytes, lone CR and CRLF line ends, bidi overrides,
//! zero-width and homoglyph keywords, and keyword-heavy statement forms.
//! Invalid UTF-8 cannot reach the parser as `&str`, so it is not exercised here.

use std::panic;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const TIME_BOUND: Duration = Duration::from_secs(30);

fn bidi_override() -> char {
    char::from_u32(0x202E).unwrap_or('?')
}

fn zero_width_space() -> char {
    char::from_u32(0x200B).unwrap_or('?')
}

fn armenian_small_seed() -> char {
    char::from_u32(0x057D).unwrap_or('?')
}

fn cases() -> Vec<(&'static str, String)> {
    let mut indented = String::from("run a\n");
    for depth in 1..=5000 {
        indented.push_str(&" ".repeat(depth));
        indented.push_str("then run b\n");
    }

    vec![
        ("empty", String::new()),
        ("only_whitespace", "   \n\t\n  \n".to_string()),
        (
            "deep_parentheses",
            format!("run a with text \"{}\"\n", "(".repeat(100_000)),
        ),
        ("deep_indentation", indented),
        (
            "huge_line",
            format!("run a with text \"{}\"\n", "x".repeat(4_000_000)),
        ),
        ("long_line_without_newline", "run ".repeat(200_000)),
        ("many_statements", "run a\n".repeat(50_000)),
        (
            "unclosed_raw_block",
            "run a with text \"\"\"\nline one\nline two\n".to_string(),
        ),
        ("unclosed_quote", "run a with text \"abc\n".to_string()),
        (
            "unbalanced_brackets",
            "run a with text \"[[[[{{{{\"\n))))]]]]}}}}\n".to_string(),
        ),
        ("nul_byte_inside_word", "run a\0b\n".to_string()),
        ("nul_bytes_only", "\0\0\0\n".to_string()),
        ("lone_cr_line_ends", "run a\rthen run b\r".to_string()),
        ("crlf_line_ends", "run a\r\nthen run b\r\n".to_string()),
        (
            "bidi_override_in_keyword",
            format!("run{} a\n", bidi_override()),
        ),
        (
            "bidi_override_in_name",
            format!("run a{}b\n", bidi_override()),
        ),
        (
            "zero_width_in_keyword",
            format!("ru{}n a\n", zero_width_space()),
        ),
        (
            "homoglyph_keyword",
            format!("r{}n a\n", armenian_small_seed()),
        ),
        (
            "keyword_statements_run_together",
            "use value\nuse bytes\nthen capture\nhand over\nlaunch detached\nwith temporary\nrun program file\n"
                .to_string(),
        ),
        ("trailing_backslash", "run a \\".to_string()),
    ]
}

/// Runs both parser entry points on `source` in a worker thread. Returns an error
/// string when the worker panics, or does not answer within `TIME_BOUND`.
fn check(name: &'static str, source: String) -> Result<(), String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let outcome = panic::catch_unwind(move || {
            let _ = flow::parser::parse(&source);
            let report = flow::parser::parse_collecting_errors(&source);
            assert!(
                report.program.is_some() || !report.errors.is_empty(),
                "neither a program nor an error was reported"
            );
        });
        let _ = sender.send(outcome.is_ok());
    });

    match receiver.recv_timeout(TIME_BOUND) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("{name}: parser panicked")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(format!("{name}: no result within {TIME_BOUND:?}"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(format!("{name}: worker ended without a result"))
        }
    }
}

#[test]
fn pathological_parser_inputs_never_panic_or_hang() {
    let failures: Vec<String> = cases()
        .into_iter()
        .filter_map(|(name, source)| check(name, source).err())
        .collect();
    assert!(
        failures.is_empty(),
        "pathological parser inputs failed:\n{}",
        failures.join("\n")
    );
}
