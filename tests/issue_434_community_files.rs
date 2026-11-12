//! Issue #434: the community files exist and carry the structure GitHub requires.
//!
//! These checks read the repository files as text. They do not parse YAML, because the
//! crate has no YAML dependency; the forms are also checked by GitHub when they load.

use std::fs;
use std::path::Path;

fn read_repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read {relative}: {error}"))
}

const FORMS: [&str; 2] = [
    ".github/ISSUE_TEMPLATE/port-command.yml",
    ".github/ISSUE_TEMPLATE/new-applet.yml",
];

#[test]
fn issue_forms_declare_name_description_and_body() {
    for form in FORMS {
        let text = read_repo_file(form);
        assert!(text.starts_with("name: "), "{form} must start with name:");
        assert!(
            text.contains("\ndescription: "),
            "{form} needs a description"
        );
        assert!(text.contains("\nbody:\n"), "{form} needs a body list");
        assert!(!text.contains('\t'), "{form} must not contain tabs");
    }
}

#[test]
fn every_issue_form_field_has_an_id() {
    for form in FORMS {
        let text = read_repo_file(form);
        let lines: Vec<&str> = text.lines().collect();
        let mut fields = 0;
        for (index, line) in lines.iter().enumerate() {
            if !line.starts_with("  - type: ") || line.ends_with("markdown") {
                continue;
            }
            fields += 1;
            let has_id = lines
                .get(index + 1)
                .is_some_and(|next| next.starts_with("    id: "));
            assert!(has_id, "{form} field at line {} has no id", index + 1);
        }
        assert!(fields > 0, "{form} declares no fields");
    }
}

#[test]
fn codeowners_covers_source_and_manifests() {
    let text = read_repo_file(".github/CODEOWNERS");
    let owner_lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .collect();
    for path in ["/src/", "/manifests/"] {
        assert!(
            owner_lines
                .iter()
                .any(|line| line.starts_with(path) && line.contains("@monstercameron")),
            "CODEOWNERS must assign {path} to @monstercameron"
        );
    }
    assert!(
        owner_lines
            .iter()
            .any(|line| line.starts_with("* @monstercameron")),
        "CODEOWNERS needs a default owner"
    );
}

#[test]
fn contributing_documents_review_policy_and_forms() {
    let text = read_repo_file("CONTRIBUTING.md");
    assert!(text.contains("\n## Review and merge\n"));
    assert!(text.contains("\n## Starting points\n"));
    assert!(text.contains(".github/CODEOWNERS"));
    assert!(text.contains(".github/ISSUE_TEMPLATE/port-command.yml"));
    assert!(text.contains(".github/ISSUE_TEMPLATE/new-applet.yml"));
}
