//! Statement-level syntax recovery: `parse_collecting_errors` reports several independent
//! errors in one pass and keeps `parse`'s first error and its program unchanged.

use flow::parser::{parse, parse_collecting_errors, MAX_COLLECTED_ERRORS};
use std::path::Path;

fn error_lines(source: &str) -> Vec<Option<usize>> {
    parse_collecting_errors(source)
        .errors
        .iter()
        .map(|error| error.line)
        .collect()
}

#[test]
fn three_independent_bad_statements_yield_three_errors_at_their_lines() {
    let source = concat!(
        "accept a as text\n",
        "run \"true\" |\n",
        "run \"true\" as x\n",
        "rnu \"printf\" with arguments [\"x\"]\n",
        "run \"true\" as y\n",
        "run \"true\" |\n",
    );
    let report = parse_collecting_errors(source);
    assert_eq!(report.errors.len(), 3, "{:?}", report.errors);
    assert_eq!(
        report.errors.iter().map(|e| e.line).collect::<Vec<_>>(),
        [Some(2), Some(4), Some(6)]
    );
    assert!(report.program.is_none());
    assert!(!report.truncated);
    assert_eq!(
        report.errors[0].message,
        format!("{:#}", parse(source).unwrap_err())
    );
}

#[test]
fn bad_line_inside_a_loop_body_is_one_error_and_the_statement_after_is_still_checked() {
    let source = concat!(
        "accept a as text\n",
        "for each file in files\n",
        "  run \"true\" |\n",
        "  run \"true\" as ok\n",
        "rnu \"printf\" with arguments [\"x\"]\n",
        "run \"true\" as after\n",
    );
    assert_eq!(error_lines(source), [Some(3), Some(5)]);
}

#[test]
fn unterminated_block_at_end_of_file_is_one_error_matching_parse() {
    let source = "accept a as text\nfor each file in files\n";
    let report = parse_collecting_errors(source);
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.program.is_none());
    assert_eq!(
        report.errors[0].message,
        format!("{:#}", parse(source).unwrap_err())
    );
}

#[test]
fn first_error_matches_parse_for_every_example_and_broken_input() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut sources: Vec<String> = std::fs::read_dir(&examples)
        .expect("examples directory")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "program"))
        .map(|path| std::fs::read_to_string(path).expect("example source"))
        .collect();
    assert!(!sources.is_empty(), "no .program examples found");
    for broken in [
        "accept name\n",
        "run \"true\" |\n",
        "if a is \"x\"\n",
        "accept a as text\nif a is \"x\"\n  run \"true\" |\n",
        "run \"printf\" with arguments [\"unterminated]\n",
        "on exit\n",
        "accept a as text\non exit\n",
        "rnu \"printf\" with arguments [\"x\"]\n",
        "accept a as text\nrun \"true\" |\non exit\n  run \"true\" as z\n",
    ] {
        sources.push(broken.to_string());
    }
    for source in &sources {
        let report = parse_collecting_errors(source);
        match parse(source) {
            Ok(program) => {
                assert!(report.errors.is_empty(), "{source:?}: {:?}", report.errors);
                assert_eq!(report.program, Some(program), "{source:?}");
            }
            Err(error) => {
                assert!(report.program.is_none(), "{source:?}");
                let first = report.errors.first().expect("at least one error");
                assert_eq!(first.message, format!("{error:#}"), "{source:?}");
            }
        }
    }
}

#[test]
fn inputs_without_errors_give_the_same_program_as_parse() {
    for source in [
        "accept a as text\nrun \"true\" as x\n",
        "accept a as text\nfor each file in files\n  run \"true\" as ok\nrun \"true\" as after\n",
    ] {
        let report = parse_collecting_errors(source);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!report.truncated);
        assert_eq!(report.program, Some(parse(source).unwrap()));
    }
}

#[test]
fn error_count_is_bounded_and_truncation_is_reported() {
    let source = "run \"true\" |\n".repeat(MAX_COLLECTED_ERRORS + 5);
    let report = parse_collecting_errors(&source);
    assert_eq!(report.errors.len(), MAX_COLLECTED_ERRORS);
    assert!(report.truncated);
    assert_eq!(report.errors[0].line, Some(1));
}
