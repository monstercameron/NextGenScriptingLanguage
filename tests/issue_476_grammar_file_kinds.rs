//! Issue #476: docs/grammar.md specifies stream, screen and graph files. Every
//! ```stream, ```screen and ```graph fence in that document must parse with the
//! parser for its kind, and a deliberately broken copy of each kind's first example
//! must fail, so the check cannot pass by accident.

use std::fs;
use std::path::Path;

fn grammar() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/grammar.md");
    fs::read_to_string(path).unwrap()
}

/// The bodies of fences tagged `info`, with the line where each fence opens.
fn fenced_examples(text: &str, info: &str) -> Vec<(usize, String)> {
    let opening = format!("```{info}");
    let mut examples = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        match &mut current {
            None if line.trim_end() == opening => current = Some((index + 1, String::new())),
            Some((start, body)) if line.trim_end() == "```" => {
                examples.push((*start, std::mem::take(body)));
                current = None;
            }
            Some((_, body)) => {
                body.push_str(line);
                body.push('\n');
            }
            None => {}
        }
    }
    assert!(current.is_none(), "an unterminated ```{info} fence");
    examples
}

fn assert_examples_present(kind: &str, examples: &[(usize, String)]) {
    assert!(
        !examples.is_empty(),
        "docs/grammar.md has no ```{kind} example, so the {kind} grammar is unchecked"
    );
}

/// Parse every example with `parse`, collecting each parser message with its line.
fn failures_for(
    examples: &[(usize, String)],
    parse: impl Fn(&str) -> Result<(), String>,
) -> Vec<String> {
    let mut failures = Vec::new();
    for (line, source) in examples {
        if let Err(error) = parse(source) {
            let error = error.replace('\n', " | ");
            failures.push(format!(
                "expected a parse, got error. example at line {line}: {error} [source: {:?}]",
                source
            ));
        }
    }
    failures
}

fn stream_check(source: &str) -> Result<(), String> {
    flow::stream::parse(source)
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
}

#[cfg(all(feature = "screens", not(target_arch = "wasm32")))]
fn screen_check(source: &str) -> Result<(), String> {
    flow::ui::syntax::parse(source)
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
}

#[cfg(feature = "graph")]
fn graph_check(source: &str) -> Result<(), String> {
    // Graph nodes name applets, so the examples resolve against the bundled catalog.
    // The manifests are written to a temp directory, as tests/issue_738_*.rs does.
    let manifests = tempfile::tempdir().map_err(|error| error.to_string())?;
    let index = flow::bundled_manifests::index().map_err(|error| format!("{error:#}"))?;
    for (position, entry) in index.entries().iter().enumerate() {
        let body = index.body(position).map_err(|error| format!("{error:#}"))?;
        std::fs::write(manifests.path().join(entry.file()), body)
            .map_err(|error| error.to_string())?;
    }
    let registry = flow::manifest::Registry::from_bundled(manifests.path())
        .map_err(|error| format!("{error:#}"))?;
    flow::graph::parse(source, &registry)
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
}

#[test]
fn every_stream_example_parses_with_the_stream_parser() {
    let examples = fenced_examples(&grammar(), "stream");
    assert_examples_present("stream", &examples);
    let failures = failures_for(&examples, stream_check);
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

#[cfg(all(feature = "screens", not(target_arch = "wasm32")))]
#[test]
fn every_screen_example_parses_with_the_screen_parser() {
    let examples = fenced_examples(&grammar(), "screen");
    assert_examples_present("screen", &examples);
    let failures = failures_for(&examples, screen_check);
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

#[cfg(feature = "graph")]
#[test]
fn every_graph_example_parses_with_the_graph_parser() {
    let examples = fenced_examples(&grammar(), "graph");
    assert_examples_present("graph", &examples);
    let failures = failures_for(&examples, graph_check);
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

/// Break the first stream example by misspelling its sink. The parser must reject it,
/// so a grammar that drifts from the parser fails the check above.
#[test]
fn stream_check_rejects_a_drifted_example() {
    let examples = fenced_examples(&grammar(), "stream");
    let (_, source) = examples.first().expect("a stream example");
    assert!(
        source.contains("show each line"),
        "the mutation target is in the example"
    );
    let broken = source.replacen("show each line", "shwo each line", 1);
    let error = stream_check(&broken).expect_err("a misspelt sink must not parse");
    assert!(
        error.contains("show each line"),
        "the stream parser should name the sink it expected, got: {error}"
    );
}

/// Break the first screen example by misspelling its component declaration.
#[cfg(all(feature = "screens", not(target_arch = "wasm32")))]
#[test]
fn screen_check_rejects_a_drifted_example() {
    let examples = fenced_examples(&grammar(), "screen");
    let (_, source) = examples.first().expect("a screen example");
    assert!(
        source.contains("define a program named"),
        "the mutation target is in the example"
    );
    let broken = source.replacen("define a program named", "define a progam named", 1);
    let error = screen_check(&broken).expect_err("a misspelt declaration must not parse");
    assert!(
        error.contains("line"),
        "the screen parser should report the line of the bad declaration, got: {error}"
    );
}

/// Break the first graph example by misspelling its closing line.
#[cfg(feature = "graph")]
#[test]
fn graph_check_rejects_a_drifted_example() {
    let examples = fenced_examples(&grammar(), "graph");
    let (_, source) = examples.first().expect("a graph example");
    assert!(
        source.contains("end graph"),
        "the mutation target is in the example"
    );
    let broken = source.replacen("end graph", "end grph", 1);
    let error = graph_check(&broken).expect_err("a misspelt closing line must not parse");
    assert!(
        error.contains("end graph"),
        "the graph parser should name the closing line it expected, got: {error}"
    );
}
