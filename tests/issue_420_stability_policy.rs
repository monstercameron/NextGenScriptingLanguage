//! Issue #420: the stability policy is linked from the README and exists, and
//! every published JSON Schema carries a versioned file name whose `$id` names
//! that file and which is listed in the policy.

use std::path::Path;

fn project() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn policy_is_linked_from_readme_and_exists() {
    let readme = std::fs::read_to_string(project().join("README.md")).expect("reading README.md");
    assert!(
        readme.contains("docs/stability.md"),
        "README.md must link docs/stability.md"
    );
    assert!(project().join("docs/stability.md").is_file());
}

#[test]
fn every_published_schema_is_versioned_and_listed_in_the_policy() {
    let policy =
        std::fs::read_to_string(project().join("docs/stability.md")).expect("reading stability.md");
    let mut checked = 0;
    for entry in std::fs::read_dir(project().join("docs/schemas")).expect("listing docs/schemas") {
        let path = entry.expect("reading a docs/schemas entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 schema file name")
            .to_string();
        if !name.ends_with(".json") {
            continue;
        }
        let stem = name
            .strip_suffix(".schema.json")
            .unwrap_or_else(|| panic!("{name}: schema file names end in .schema.json"));
        let version = stem.rsplit_once(".v").map(|(_, version)| version);
        assert!(
            matches!(version, Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())),
            "{name}: file name must carry a .vN version"
        );
        let text = std::fs::read_to_string(&path).expect("reading schema file");
        let schema: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
        let id = schema
            .get("$id")
            .and_then(|id| id.as_str())
            .unwrap_or_default();
        assert_eq!(
            id,
            format!("https://execute.invalid/schemas/{name}"),
            "{name}: $id must name the versioned file"
        );
        assert!(
            policy.contains(&name),
            "{name} is published but not listed in docs/stability.md"
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "expected at least six published schemas, found {checked}"
    );
}
