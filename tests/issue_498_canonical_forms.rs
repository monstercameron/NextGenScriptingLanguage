//! Issue #498: the "Canonical forms" table in docs/applet-contracts.md may name
//! only applets that exist in the bundled registry, so the table cannot drift.

use std::path::Path;

const HEADING: &str = "## Canonical forms\n";

fn canonical_section() -> String {
    let doc = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("applet-contracts.md");
    let text = std::fs::read_to_string(&doc).expect("read docs/applet-contracts.md");
    let start = text.find(HEADING).expect("canonical forms heading");
    let after = &text[start + HEADING.len()..];
    let body = after.find("\n## ").map_or(after, |end| &after[..end]);
    let authoring = text
        .find("## Authoring an applet: a worked example")
        .expect("authoring guide heading");
    assert!(
        start < authoring,
        "the canonical forms table must come before the authoring guide"
    );
    body.to_owned()
}

/// The applet names written as `run NAME` in one table cell. A quoted name
/// (`run "sort"`) is a direct native call that bypasses the catalog, so it is
/// not an applet name and is skipped.
fn applet_calls(cell: &str) -> Vec<String> {
    let mut calls = Vec::new();
    for (at, _) in cell.match_indices("run ") {
        let name: String = cell[at + "run ".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        if name.starts_with(|c: char| c.is_ascii_lowercase()) {
            calls.push(name);
        }
    }
    calls
}

#[test]
fn every_applet_named_in_the_canonical_table_exists_in_the_bundled_registry() {
    let body = canonical_section();
    let index = flow::bundled_manifests::index().expect("bundled index");

    let mut rows = 0;
    let mut names = Vec::new();
    for line in body.lines().filter(|line| line.starts_with('|')) {
        // The first two lines are the header and the separator.
        if line.starts_with("| Operation") || line.starts_with("| ---") {
            continue;
        }
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        assert_eq!(cells.len(), 4, "table row must have four cells: {line}");
        rows += 1;
        names.extend(applet_calls(cells[1]));
        names.extend(applet_calls(cells[2]));
    }

    assert!(
        rows >= 8,
        "expected at least eight canonical rows, found {rows}"
    );
    assert!(!names.is_empty(), "the table names no applet calls");
    for name in &names {
        assert!(
            index.lookup(name).is_some(),
            "docs/applet-contracts.md names applet `{name}`, which is not in the bundled registry"
        );
    }
}
