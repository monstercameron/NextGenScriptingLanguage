//! Keeps the public language and runtime references synchronized with the parser.

use std::fs;
use std::path::Path;

fn doc(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

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
    assert!(current.is_none(), "unterminated ```program fence");
    examples
}

#[test]
fn every_reference_program_example_parses() {
    let mut failures = Vec::new();
    let mut count = 0;
    for path in ["docs/language.md", "docs/runtime-reference.md"] {
        let examples = program_examples(&doc(path));
        count += examples.len();
        for (line, source) in examples {
            if let Err(error) = flow::parser::parse(&source) {
                failures.push(format!("{path}:{line}: {error:#}\n{source}"));
            }
        }
    }
    assert!(
        count >= 8,
        "expected examples from both references, found {count}"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

#[test]
fn language_reference_has_no_stale_release_caveats() {
    let reference = doc("docs/language.md").to_lowercase();
    for stale in ["0.8.0", "source candidate", "current source"] {
        assert!(
            !reference.contains(stale),
            "language reference contains {stale:?}"
        );
    }
    assert!(!reference.contains("failure contains the first error"));
    assert!(!reference.contains("no line/column spans are guessed"));
}

#[test]
fn moved_reference_sections_keep_legacy_anchors() {
    let language = doc("docs/language.md");
    let runtime = doc("docs/runtime-reference.md");
    for (anchor, heading) in [
        (
            "finite-stdin-and-unix-composition",
            "Finite stdin and Unix composition",
        ),
        ("program-inputs", "Program inputs"),
        ("agent-interface-discovery", "Agent interface discovery"),
        ("machine-readable-checks", "Machine-readable checks"),
        (
            "review-a-plan-before-it-runs",
            "Review a plan before it runs",
        ),
        ("warm-daemon---daemon", "Warm daemon (`--daemon`)"),
        ("recover-after-a-failure", "Recover after a failure"),
        ("per-element-checkpoints", "Per-element checkpoints"),
        ("bound-command-lifetimes", "Bound command lifetimes"),
    ] {
        assert!(
            language.contains(&format!("id=\"{anchor}\"")),
            "missing alias {anchor}"
        );
        assert!(
            language.contains(&format!("runtime-reference.md#{anchor}")),
            "missing link {anchor}"
        );
        assert!(
            runtime.contains(&format!("## {heading}"))
                || runtime.contains(&format!("### {heading}")),
            "missing target heading {heading}"
        );
    }
}
