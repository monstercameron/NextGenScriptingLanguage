//! Issue #364: every machine protocol identifier the Rust sources emit (`execute.NAME/N`)
//! must have a schema file under `docs/schemas/`, or an explicit entry in
//! `WITHOUT_SCHEMA` that says why it has none. A new protocol id added to `src/`
//! without either fails this test.
//!
//! The scan is plain text over `src/**/*.rs`, so it needs no cargo features.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn project() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Protocol ids (`name/version`, spelled as the source spells them) that have no schema
/// file yet. Remove an entry when its schema is added under `docs/schemas/`.
const WITHOUT_SCHEMA: &[(&str, &str)] = &[
    ("applet/1", "schema not written yet (#364 open)"),
    ("applet-matches/1", "schema not written yet (#364 open)"),
    ("applet-search/1", "schema not written yet (#364 open)"),
    ("applets/1", "schema not written yet (#364 open)"),
    ("applets-discovery/1", "schema not written yet (#364 open)"),
    ("attach/1", "schema not written yet (#364 open)"),
    ("daemon/1", "schema not written yet (#364 open)"),
    (
        "elevated_applet/2",
        "only elevated-applet.v1 is published (#364 open)",
    ),
    ("graph/1", "schema not written yet (#364 open)"),
    ("graph-run/1", "schema not written yet (#364 open)"),
    ("help/1", "schema not written yet (#364 open)"),
    ("live/1", "schema not written yet (#364 open)"),
    ("model/1", "schema not written yet (#364 open)"),
    ("model/2", "schema not written yet (#364 open)"),
    ("model-request/1", "schema not written yet (#364 open)"),
    ("modules/1", "schema not written yet (#364 open)"),
    ("panic/1", "schema not written yet (#364 open)"),
    ("policy-audit/1", "schema not written yet (#364 open)"),
    (
        "policy-audit-seal/1",
        "seal domain label used when hashing (src/record.rs); no schema yet",
    ),
    (
        "policy-audit-seal/2",
        "seal domain label used when hashing (src/record.rs); no schema yet",
    ),
    ("record/1", "schema not written yet (#364 open)"),
    ("record-seal/1", "schema not written yet (#364 open)"),
    ("repair/1", "schema not written yet (#364 open)"),
    ("run-parts/1", "schema not written yet (#364 open)"),
    ("show-run/1", "schema not written yet (#364 open)"),
    ("test/1", "schema not written yet (#364 open)"),
    (
        "what_if/1",
        "legacy plan output still emitted (src/agent/what_if.rs); only what-if.v2 is published",
    ),
];

/// Protocol ids whose schema is published outside `docs/schemas/`.
const SCHEMA_OVERRIDES: &[(&str, &str)] = &[("check/1", "docs/check-result.schema.json")];

/// The file a protocol id's schema must live in: `docs/schemas/NAME.vN.schema.json`
/// with underscores in the name written as hyphens, unless overridden.
fn expected_schema(id: &str, root: &Path) -> PathBuf {
    if let Some(&(_, path)) = SCHEMA_OVERRIDES.iter().find(|&&(key, _)| key == id) {
        return root.join(path);
    }
    let (name, version) = id.split_once('/').expect("protocol id has a version");
    root.join("docs").join("schemas").join(format!(
        "{}.v{}.schema.json",
        name.replace('_', "-"),
        version
    ))
}

/// Records every `execute.NAME/N` literal in `text`.
fn scan_text(text: &str, found: &mut BTreeSet<String>) {
    let mut rest = text;
    while let Some(at) = rest.find("execute.") {
        rest = &rest[at + "execute.".len()..];
        let name_len = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c == '_' || c == '-' || c == '.'))
            .unwrap_or(rest.len());
        let name = &rest[..name_len];
        if let Some(after_slash) = rest[name_len..].strip_prefix('/') {
            let digits = after_slash
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after_slash.len());
            if !name.is_empty() && digits > 0 {
                found.insert(format!("{name}/{}", &after_slash[..digits]));
            }
        }
    }
}

/// Walks `dir` and records the protocol ids in every `.rs` file beneath it.
fn emitted_protocol_ids(dir: &Path, found: &mut BTreeSet<String>) {
    for entry in fs::read_dir(dir).expect("readable source directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            emitted_protocol_ids(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            if let Ok(text) = fs::read_to_string(&path) {
                scan_text(&text, found);
            }
        }
    }
}

/// Problems with schema coverage: an emitted id with neither a schema file nor an
/// exemption, an exemption that now has a schema, and an exemption no source emits.
fn coverage_problems(
    emitted: &BTreeSet<String>,
    root: &Path,
    exempt: &[(&str, &str)],
) -> Vec<String> {
    let mut problems = Vec::new();
    for id in emitted {
        let schema = expected_schema(id, root);
        let has_schema = schema.is_file();
        let exempted = exempt.iter().any(|&(key, _)| key == id.as_str());
        if !has_schema && !exempted {
            problems.push(format!(
                "{id}: no schema at {} and not listed in WITHOUT_SCHEMA",
                schema.display()
            ));
        }
        if has_schema && exempted {
            problems.push(format!(
                "{id}: has a schema now; remove it from WITHOUT_SCHEMA"
            ));
        }
    }
    for &(key, _) in exempt {
        if !emitted.contains(key) {
            problems.push(format!(
                "{key}: listed in WITHOUT_SCHEMA but no source emits it"
            ));
        }
    }
    problems
}

#[test]
fn every_emitted_protocol_id_has_a_schema_or_a_reasoned_exemption() {
    let mut emitted = BTreeSet::new();
    emitted_protocol_ids(&project().join("src"), &mut emitted);
    assert!(
        emitted.contains("check/1") && emitted.contains("inspect/1"),
        "the source scan found too few protocol ids: {emitted:?}"
    );
    let problems = coverage_problems(&emitted, project(), WITHOUT_SCHEMA);
    assert!(
        problems.is_empty(),
        "protocol schema coverage problems:\n{}",
        problems.join("\n")
    );
}

#[test]
fn an_id_with_neither_a_schema_nor_an_exemption_is_reported() {
    let mut emitted = BTreeSet::new();
    emitted.insert("imaginary-protocol/1".to_string());
    emitted.insert("check/1".to_string());
    let problems = coverage_problems(&emitted, project(), &[]);
    assert_eq!(problems.len(), 1, "unexpected problems: {problems:?}");
    assert!(problems[0].starts_with("imaginary-protocol/1: no schema"));
}

#[test]
fn the_scanner_reads_names_versions_and_ignores_other_text() {
    let mut found = BTreeSet::new();
    scan_text(
        "a \"execute.model/2\" b \"execute.what_if/1\" execute.run/ execute.exe x",
        &mut found,
    );
    let expected: BTreeSet<String> = ["model/2", "what_if/1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(found, expected);
}
