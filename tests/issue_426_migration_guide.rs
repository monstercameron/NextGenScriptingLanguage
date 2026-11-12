//! Keeps docs/migrating-from-bash.md honest: every ```program example in the guide
//! parses with the real parser, and the guide maps at least forty idioms.

use std::fs;
use std::path::Path;

fn guide() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/migrating-from-bash.md");
    fs::read_to_string(path).unwrap()
}

/// The bodies of ```program fences, with the line where each fence opens.
fn program_examples(text: &str) -> Vec<(usize, String)> {
    let mut examples = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        match &mut current {
            None if line.trim_end() == "```program" => current = Some((index + 1, String::new())),
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
    assert!(current.is_none(), "an unterminated ```program fence");
    examples
}

#[test]
fn every_program_example_in_the_guide_parses() {
    let examples = program_examples(&guide());
    assert!(
        examples.len() >= 20,
        "expected the guide's examples, found {}",
        examples.len()
    );
    let mut failures = Vec::new();
    for (line, source) in &examples {
        if let Err(error) = flow::parser::parse(source) {
            failures.push(format!("example at line {line}: {error:#}\n{source}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failing examples: {}",
        failures.len(),
        failures.join(" ### ").replace('\n', " | ")
    );
}

#[test]
fn the_guide_maps_at_least_forty_idioms() {
    // Each idiom is one table row with four cells: idiom, Execute form, status, notes.
    // The status cell must be one of the five status words the guide defines.
    let statuses = ["supported", "applet", "partial", "missing", "by design"];
    let rows = guide()
        .lines()
        .filter(|line| {
            let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
            let cells: Vec<&str> = inner.split(" | ").map(str::trim).collect();
            cells.len() == 4 && statuses.contains(&cells[2])
        })
        .count();
    assert!(
        rows >= 40,
        "the guide maps {rows} idioms, expected at least 40"
    );
}

#[test]
fn the_extractor_finds_each_fence_once() {
    let sample = "text\n```program\nrun \"true\"\n```\n```bash\nnot a program\n```\n";
    let examples = program_examples(sample);
    assert_eq!(examples, vec![(2, "run \"true\"\n".to_owned())]);
}
