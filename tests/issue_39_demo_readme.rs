//! Issue #39: the flagship demo is the first section of the README, and the walkthrough
//! uses a run record. Library-level: the demo program parses, and the documents point at
//! each other. The walkthrough itself runs in `tests/demo_walkthrough.rs` (Unix only).

use std::fs;
use std::path::Path;

fn repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {relative}: {error}"))
}

#[test]
fn the_demo_program_parses_and_declares_its_five_inputs() {
    let source = repo_file("examples/demo/theme-refresh.program");
    let program = flow::parser::parse(&source).expect("the demo program must parse");
    let inputs: Vec<&str> = program
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect();
    assert_eq!(
        inputs,
        [
            "theme_name",
            "themes_root",
            "current_link",
            "templates",
            "values_json"
        ]
    );
    assert!(
        source.starts_with("# execute-policy: allow reads writes process undeclared\n"),
        "the demo must declare its policy header"
    );
}

#[test]
fn the_readme_opens_with_a_link_to_the_demo() {
    let readme = repo_file("README.md");
    let demo_link = readme
        .find("](examples/demo/README.md)")
        .expect("the README must link examples/demo/README.md");
    let quickstart = readme
        .find("\n## Quickstart")
        .expect("the README must keep its Quickstart section");
    assert!(
        demo_link < quickstart,
        "the demo link must come before the Quickstart section"
    );
    // The first link to the demo must be the demo's README, not another file in examples/demo.
    let first_demo_link = readme
        .find("examples/demo/")
        .expect("the README must mention the demo folder");
    assert!(
        readme[first_demo_link..].starts_with("examples/demo/README.md"),
        "the first demo link must point at examples/demo/README.md"
    );
}

#[test]
fn the_walkthrough_records_the_run_and_checks_the_record() {
    let script = repo_file("examples/demo/walkthrough.sh");
    assert!(
        script.contains("--record \"$record\""),
        "the confirm step must use --record"
    );
    assert!(
        script.contains("repair --from-record \"$record\""),
        "the repair must read the record"
    );
    assert!(
        script.contains("resume \"$program\" --from-record \"$record\""),
        "the resume must read the record"
    );
    assert!(
        script.contains("\"$record/run.json\""),
        "the walkthrough must check that the run record exists"
    );
}
