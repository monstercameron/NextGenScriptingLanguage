//! Issue #476: stream and screen parse errors carry a stable code, a 1-based line and,
//! where the failing token is known, a 1-based column. These tests first pin the
//! messages `parse` returns today, so the structured entry point cannot change them.

/// The `{:#}` form, which the CLI and the first-half grammar test print.
fn chain(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

/// The outermost layer alone, which `{}` shows.
fn outer(error: &anyhow::Error) -> String {
    format!("{error}")
}

#[test]
fn stream_parse_messages_are_pinned() {
    let cases: [(&str, &str, &str); 8] = [
        (
            "  stream from tail\n",
            "line 1: stream statements must not be indented",
            "line 1: stream statements must not be indented",
        ),
        (
            "tail\n",
            "line 1: expected `stream from <applet>`",
            "line 1: expected `stream from <applet>`",
        ),
        (
            "stream from\nshow each line\n",
            "line 1: stream source requires an applet phrase",
            "line 1: stream source requires an applet phrase",
        ),
        (
            "stream from tail\n",
            "stream program requires final sink `show each line`",
            "stream program requires final sink `show each line`",
        ),
        (
            "stream from tail\nshow each line\nshow each line\n",
            "line 3: the final sink may appear only once",
            "line 3: the final sink may appear only once",
        ),
        (
            "stream from tail\nshow each line\ntake the first 2 lines\n",
            "line 3: statements cannot follow the final sink",
            "line 3: statements cannot follow the final sink",
        ),
        (
            "stream from tail\ntake the first 0 lines\nshow each line\n",
            "line 2: line count must be greater than zero",
            "line 2",
        ),
        (
            "stream from tail\npipe to \"tr\nshow each line\n",
            "line 2: unterminated quoted string",
            "line 2",
        ),
    ];
    for (source, expected_chain, expected_outer) in cases {
        let error = flow::stream::parse(source).expect_err(source);
        assert_eq!(chain(&error), expected_chain, "source: {source:?}");
        assert_eq!(outer(&error), expected_outer, "source: {source:?}");
    }
}

#[cfg(all(feature = "screens", not(target_arch = "wasm32")))]
#[test]
fn screen_parse_messages_are_pinned() {
    let cases: [(&str, &str, &str); 6] = [
        (
            "open a screen titled \"x\"\n\tplace a on the left\n",
            "line 2: tabs are not allowed",
            "line 2: tabs are not allowed",
        ),
        (
            "open a screen titled \"x\"\nfrobnicate\n",
            "line 2: unknown screen declaration",
            "line 2",
        ),
        (
            "divide the screen into two columns\n",
            "a screen needs a title and one or two placed components",
            "a screen needs a title and one or two placed components",
        ),
        (
            "open a screen titled \"x\"\n  place a on the left\n",
            "line 2: unexpected top-level indentation",
            "line 2",
        ),
        (
            "open a screen titled \"x\"\nplace a on the left\ndefine a program named a\n   show a heading named h\n",
            "line 3: line 4: component declarations need two spaces",
            "line 3",
        ),
        (
            "open a screen titled \"x\"\nplace a on the left\ndefine a program named a\n  show a frob named h\n",
            "line 3: line 4: unsupported widget kind",
            "line 3",
        ),
    ];
    for (source, expected_chain, expected_outer) in cases {
        let error = flow::ui::syntax::parse(source).expect_err(source);
        assert_eq!(chain(&error), expected_chain, "source: {source:?}");
        assert_eq!(outer(&error), expected_outer, "source: {source:?}");
    }
}

/// `parse_detailed` must agree with `parse`: the same text, and the same first error.
#[test]
fn stream_errors_carry_code_line_and_column() {
    // (source, code, line, column)
    let cases: [(&str, &str, usize, Option<usize>); 11] = [
        ("  stream from tail\n", "parse.indentation", 1, Some(1)),
        ("tail\n", "parse.syntax", 1, Some(1)),
        ("stream from\nshow each line\n", "parse.syntax", 1, Some(12)),
        ("stream from tail\n", "parse.syntax", 1, None),
        (
            "stream from tail\nshow each line\nshow each line\n",
            "parse.syntax",
            3,
            Some(1),
        ),
        (
            "stream from tail\nshow each line\ntake the first 2 lines\n",
            "parse.syntax",
            3,
            Some(1),
        ),
        (
            "stream from tail\ntake the first 0 lines\nshow each line\n",
            "check.range",
            2,
            Some(16),
        ),
        (
            "stream from tail\ntake the first nope lines\nshow each line\n",
            "parse.syntax",
            2,
            Some(16),
        ),
        (
            "stream from tail\ntake the first 99999999999999999999999 lines\nshow each line\n",
            "check.range",
            2,
            Some(16),
        ),
        (
            "stream from tail\npipe to \"tr\nshow each line\n",
            "parse.lexical",
            2,
            Some(9),
        ),
        (
            "stream from \"unterminated\nshow each line\n",
            "parse.lexical",
            1,
            Some(13),
        ),
    ];
    for (source, code, line, column) in cases {
        let error = flow::stream::parse_detailed(source).expect_err(source);
        assert_eq!(
            (error.code, error.line, error.column),
            (code, line, column),
            "source: {source:?}\nmessage: {}",
            error.message
        );
        let plain = flow::stream::parse(source).expect_err(source);
        assert_eq!(
            format!("{plain:#}"),
            error.message,
            "parse and parse_detailed disagree for {source:?}"
        );
    }
}

#[cfg(all(feature = "screens", not(target_arch = "wasm32")))]
#[test]
fn screen_errors_carry_code_line_and_column() {
    let header = "open a screen titled \"x\"\nplace a on the left\ndefine a program named a\n";
    // (source, code, line, column)
    let cases: Vec<(String, &str, usize, Option<usize>)> = vec![
        (
            "open a screen titled \"x\"\n\tplace a on the left\n".into(),
            "parse.indentation",
            2,
            Some(1),
        ),
        (
            "open a screen titled \"x\"\nfrobnicate\n".into(),
            "parse.syntax",
            2,
            Some(1),
        ),
        (
            "divide the screen into two columns\n".into(),
            "parse.syntax",
            1,
            None,
        ),
        (
            "open a screen titled \"x\"\n  place a on the left\n".into(),
            "parse.indentation",
            2,
            Some(3),
        ),
        (
            "open a screen titled \"x\"\nplace a on the left\nplace a on the right\n".into(),
            "check.duplicate_name",
            3,
            Some(1),
        ),
        (
            "open a screen titled \"x\"\nplace a on the left\ngive a 5 percent of the width\n"
                .into(),
            "check.range",
            3,
            Some(1),
        ),
        (
            format!("{header}   show a heading named h\n"),
            "parse.indentation",
            4,
            Some(4),
        ),
        (
            format!("{header}  show a frob named h\n"),
            "parse.syntax",
            4,
            Some(3),
        ),
        (
            format!("{header}  show a heading named h\n  show a heading named h\n"),
            "check.duplicate_name",
            5,
            Some(3),
        ),
        (
            format!("{header}  show a heading named h\n  whenever key \"Ctrl-C\" is pressed\n    show the result in h\n"),
            "parse.syntax",
            5,
            Some(16),
        ),
        (
            format!("{header}  show a heading named h\n  whenever the program opens\n    show the result in h\n  every 5 fortnights\n    show the result in h\n"),
            "parse.syntax",
            7,
            Some(3),
        ),
    ];
    for (source, code, line, column) in cases {
        let error = flow::ui::syntax::parse_detailed(&source).expect_err(&source);
        assert_eq!(
            (error.code, error.line, error.column),
            (code, line, column),
            "source: {source:?}\nmessage: {}",
            error.message
        );
        let plain = flow::ui::syntax::parse(&source).expect_err(&source);
        assert_eq!(
            format!("{plain:#}"),
            error.message,
            "parse and parse_detailed disagree for {source:?}"
        );
    }
}

/// The interval error names the `every` line, not the synthetic `line 1` of the
/// trigger parser's own source.
#[cfg(all(feature = "screens", not(target_arch = "wasm32")))]
#[test]
fn screen_interval_error_names_the_real_line() {
    let source = "open a screen titled \"x\"\nplace a on the left\ndefine a program named a\n  show a heading named h\n  whenever the program opens\n    show the result in h\n  every 5 fortnights\n    show the result in h\n";
    let error = flow::ui::syntax::parse_detailed(source).expect_err("bad interval");
    // The enclosing `define` line wraps the component error, as it did before.
    assert!(
        error.message.starts_with("line 3: line 7: "),
        "{}",
        error.message
    );
    assert!(!error.message.contains("line 1"), "{}", error.message);
    assert_eq!((error.code, error.line), ("parse.syntax", 7));
}
