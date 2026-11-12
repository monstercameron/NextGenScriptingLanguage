//! Parity between the generated GBNF and the parser on the programs in `examples/`.
//! The grammar covers only the statement forms in `GRAMMAR_FORMS`. Every other
//! leading keyword in `examples/` is a known gap, listed in `UNCOVERED_KEYWORDS`,
//! so the gap can shrink but cannot grow without a visible change here.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use flow::parser::{gbnf_grammar, parse};

/// Leading keywords used by example statements that the generated grammar does
/// not encode yet (blocks, options and the other forms in docs/grammar.md).
/// Remove a keyword here when its form is added to `GRAMMAR_FORMS`.
const UNCOVERED_KEYWORDS: &[&str] = &[
    "connect",
    "define",
    "divide",
    "give",
    "end",
    "every",
    "execute",
    "exit",
    "finally",
    "for",
    "graph",
    "if",
    "listen",
    "on",
    "open",
    "otherwise",
    "parallel",
    "place",
    "race",
    "read",
    "repeat",
    "show",
    "stream",
    "take",
    "try",
    "catch",
    "whenever",
    "while",
];

/// Every `.program` file under `examples/`, sorted by path.
fn example_programs() -> Vec<(PathBuf, String)> {
    fn collect(directory: &Path, paths: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("examples directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect(&path, paths);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "program")
            {
                paths.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    collect(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples"),
        &mut paths,
    );
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path).expect("example program");
            (path, text)
        })
        .collect()
}

/// The first word of each unindented statement line. Blank lines, indented
/// block bodies and comment lines are not statements of their own.
fn statement_keywords(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with(|c: char| c.is_ascii_alphabetic()))
        .filter_map(|line| line.split_whitespace().next())
        .collect()
}

/// True when the generated grammar has a literal that starts with `keyword`.
fn is_covered(grammar: &str, keyword: &str) -> bool {
    grammar.contains(&format!("\"{keyword} ")) || grammar.contains(&format!("\"{keyword}\""))
}

/// True when every non-blank line is an unindented statement whose first word
/// the grammar covers, so the whole file is within the grammar's reach.
fn fully_covered(grammar: &str, text: &str) -> bool {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| {
            line.starts_with(|c: char| c.is_ascii_alphabetic())
                && line
                    .split_whitespace()
                    .next()
                    .is_some_and(|keyword| is_covered(grammar, keyword))
        })
}

#[test]
fn every_example_statement_keyword_is_covered_or_a_known_gap() {
    let grammar = gbnf_grammar();
    let known: BTreeSet<&str> = UNCOVERED_KEYWORDS.iter().copied().collect();

    let mut unknown = BTreeSet::new();
    let mut seen_uncovered = BTreeSet::new();
    for (path, text) in example_programs() {
        for keyword in statement_keywords(&text) {
            if is_covered(&grammar, keyword) {
                continue;
            }
            seen_uncovered.insert(keyword.to_owned());
            if !known.contains(keyword) {
                unknown.insert(format!("{keyword} (first in {})", path.display()));
            }
        }
    }
    eprintln!(
        "example keywords not covered by the generated grammar: {}",
        seen_uncovered
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        unknown.is_empty(),
        "example statements start with keywords the grammar does not cover and \
         UNCOVERED_KEYWORDS does not list: {unknown:?}"
    );

    let stale: Vec<&str> = UNCOVERED_KEYWORDS
        .iter()
        .copied()
        .filter(|keyword| is_covered(&grammar, keyword))
        .collect();
    assert!(
        stale.is_empty(),
        "these keywords are now covered by the grammar; remove them from UNCOVERED_KEYWORDS: {stale:?}"
    );
}

#[test]
fn programs_made_only_of_covered_forms_are_accepted_by_the_parser() {
    let grammar = gbnf_grammar();
    let mut checked = 0;
    let mut rejected = Vec::new();
    for (path, text) in example_programs() {
        if !fully_covered(&grammar, &text) {
            continue;
        }
        checked += 1;
        if let Err(error) = parse(&text) {
            rejected.push(format!("{}: {error}", path.display()));
        }
    }
    assert!(
        checked > 0,
        "no example program is made only of covered forms"
    );
    assert!(
        rejected.is_empty(),
        "the parser rejects example programs the grammar covers: {rejected:#?}"
    );
}
