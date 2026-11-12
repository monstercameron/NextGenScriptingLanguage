//! Issue #404: shell-quoted export lines for environment-producing programs.

use flow::env_export::{bash_export_line, bash_export_lines, sh_single_quote, ExportError};

#[test]
fn values_with_spaces_quotes_and_newlines_are_single_quoted() {
    assert_eq!(sh_single_quote("two words"), "'two words'");
    assert_eq!(sh_single_quote("it's"), "'it'\\''s'");
    assert_eq!(sh_single_quote("say \"hi\""), "'say \"hi\"'");
    assert_eq!(sh_single_quote("first\nsecond"), "'first\nsecond'");
}

#[test]
fn shell_syntax_in_values_is_literal() {
    assert_eq!(sh_single_quote("$(rm -rf x); `id`"), "'$(rm -rf x); `id`'");
}

#[test]
fn bash_export_line_writes_one_assignment() {
    let line = bash_export_line("LANG", "C.UTF-8").unwrap();
    assert_eq!(line, "export LANG='C.UTF-8'");
}

#[test]
fn bash_export_line_rejects_bad_names() {
    for name in ["", "1X", "A B", "A=B", "X;rm"] {
        assert_eq!(
            bash_export_line(name, "v"),
            Err(ExportError::InvalidName(name.to_string())),
            "name {name:?} should be refused"
        );
    }
}

#[test]
fn bash_export_lines_quotes_every_value_and_ends_each_line() {
    let out = bash_export_lines([("A", "x y"), ("B", "q\"'\nz")]).unwrap();
    assert_eq!(out, "export A='x y'\nexport B='q\"'\\''\nz'\n");
}

#[test]
fn bash_export_lines_fails_on_first_bad_name() {
    assert!(bash_export_lines([("OK", "1"), ("bad name", "2")]).is_err());
}
