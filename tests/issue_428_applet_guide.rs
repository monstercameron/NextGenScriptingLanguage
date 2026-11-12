//! Keeps the worked-applet section of `docs/applet-contracts.md` in step with the
//! manifest parser: every fenced ```json block in that section must load.

use flow::manifest::Registry;

const CONTRACTS: &str = include_str!("../docs/applet-contracts.md");
const HEADING: &str = "## Authoring an applet: a worked example";

fn authoring_section() -> &'static str {
    let start = CONTRACTS
        .find(HEADING)
        .expect("docs/applet-contracts.md has the authoring section heading");
    &CONTRACTS[start..]
}

/// Bodies of the ```json fences in `section`. Other fenced languages are skipped.
fn json_blocks(section: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_json = false;
    let mut current = String::new();
    for line in section.lines() {
        if in_json {
            if line == "```" {
                blocks.push(std::mem::take(&mut current));
                in_json = false;
            } else {
                current.push_str(line);
                current.push('\n');
            }
        } else if line == "```json" {
            in_json = true;
        }
    }
    assert!(
        !in_json,
        "unterminated ```json fence in the authoring section"
    );
    blocks
}

fn manifest_name(block: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(block).expect("block is JSON");
    value["name"]
        .as_str()
        .expect("manifest block has a string name")
        .to_owned()
}

#[test]
fn authoring_section_manifests_load_through_the_parser() {
    let blocks = json_blocks(authoring_section());
    assert_eq!(blocks.len(), 1, "the worked example has one manifest block");
    for block in &blocks {
        let registry = Registry::from_sources(&[block.as_str()])
            .unwrap_or_else(|error| panic!("guide manifest rejected: {error:#}"));
        let name = manifest_name(block);
        assert_eq!(name, "example.clock_format");
        registry
            .resolve(&name)
            .unwrap_or_else(|error| panic!("guide manifest {name} not resolvable: {error:#}"));
    }
}

#[test]
fn authoring_section_manifest_loads_from_a_directory() {
    let block = json_blocks(authoring_section())
        .into_iter()
        .next()
        .expect("worked example manifest");
    let dir = tempfile::tempdir().expect("temporary manifest directory");
    std::fs::write(dir.path().join("example.clock_format.json"), &block)
        .expect("write guide manifest");
    let registry = Registry::load(dir.path())
        .unwrap_or_else(|error| panic!("guide manifest directory rejected: {error:#}"));
    registry
        .resolve("example.clock_format")
        .unwrap_or_else(|error| panic!("loaded guide manifest not resolvable: {error:#}"));
}
