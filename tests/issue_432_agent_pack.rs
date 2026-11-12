//! Keeps the agent onboarding pack honest: every ```program example in
//! docs/agents/execute.md parses with the real parser, the pack files exist,
//! llms.txt links to them, and the card stays within its size budget.

use std::fs;
use std::path::Path;

fn read_repo(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
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
fn every_agent_card_example_parses_with_the_real_parser() {
    let examples = program_examples(&read_repo("docs/agents/execute.md"));
    assert!(
        examples.len() >= 8,
        "expected the card's examples, found {}",
        examples.len()
    );
    let mut failures = Vec::new();
    for (line, source) in &examples {
        if let Err(error) = flow::parser::parse(source) {
            failures.push(format!("example at line {line}: {error:#}\n{source}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

#[test]
fn agent_card_stays_within_its_token_budget() {
    // Roughly four characters per token; the issue asks for under 4k tokens.
    let card = read_repo("docs/agents/execute.md");
    assert!(
        card.len() < 4 * 4000,
        "docs/agents/execute.md is {} bytes, over the budget",
        card.len()
    );
}

#[test]
fn llms_txt_links_the_pack_and_the_reference() {
    let llms = read_repo("llms.txt");
    for target in [
        "docs/agents/execute.md",
        "docs/agents/execute-port-skill.md",
        "docs/language.md",
        "docs/grammar.md",
    ] {
        assert!(llms.contains(target), "llms.txt does not link {target}");
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(target).is_file(),
            "{target} is linked from llms.txt but does not exist"
        );
    }
}

#[test]
fn pack_does_not_document_a_compare_command_that_does_not_exist() {
    for file in [
        "docs/agents/execute.md",
        "docs/agents/execute-port-skill.md",
    ] {
        let text = read_repo(file);
        assert!(
            !text.contains("execute compare"),
            "{file} names `execute compare`, which the binary does not provide"
        );
    }
}
