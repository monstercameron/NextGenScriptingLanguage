//! Stage-1 failure-family triage for issue #383. `examples/omarchy-triage/triage.program` must
//! parse and pass the static checker with no errors, and every detector in `detectors.json`
//! must appear as a quoted phrase in the program. Applying the detector table in order to the
//! fixtures must give the expected family, and a clean log must give none. The program itself
//! is not run here: the binary's default manifest directory is refused by the trust gate on
//! the machine this was written on.

use flow::manifest::Registry;
use std::path::{Path, PathBuf};

fn triage_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/omarchy-triage")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The detector table as `(family, phrase)` pairs, in table order.
fn detectors() -> Vec<(String, String)> {
    let table: serde_json::Value =
        serde_json::from_str(&read(&triage_dir().join("detectors.json"))).unwrap();
    table
        .as_array()
        .expect("detectors.json is an array")
        .iter()
        .map(|entry| {
            assert_eq!(entry["match"], "fixed", "stage 1 uses fixed phrases only");
            (
                entry["family"].as_str().unwrap().to_string(),
                entry["phrase"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The family of the first detector, in table order, whose phrase occurs in the log text.
fn triage(log: &str, table: &[(String, String)]) -> Option<String> {
    table
        .iter()
        .find(|(_, phrase)| log.contains(phrase.as_str()))
        .map(|(family, _)| family.clone())
}

#[test]
fn triage_program_parses_and_passes_static_checks() {
    let path = triage_dir().join("triage.program");
    let source = read(&path);
    flow::parser::parse(&source).unwrap_or_else(|error| panic!("{error:#}"));
    let diagnostics =
        flow::diagnostics::analyze_source_at(&source, Some(&path), &Registry::default());
    let errors: Vec<String> = diagnostics
        .iter()
        .map(|diagnostic| serde_json::to_value(diagnostic).expect("diagnostic serializes"))
        .filter(|value| value["severity"] == "error")
        .map(|value| value.to_string())
        .collect();
    assert!(errors.is_empty(), "static errors: {errors:?}");
}

#[test]
fn every_detector_phrase_is_in_the_program() {
    let source = read(&triage_dir().join("triage.program"));
    for (family, phrase) in detectors() {
        assert!(
            source.contains(&format!("\"{phrase}\"")),
            "detector {family} phrase is not in triage.program: {phrase}"
        );
    }
}

#[test]
fn fixtures_give_the_expected_family_and_clean_log_gives_none() {
    let table = detectors();
    let fixtures = [
        ("pacman-conflict.log", Some("package-conflict")),
        ("disk-full.log", Some("disk-space")),
    ];
    for (name, expected) in fixtures {
        let log = read(&triage_dir().join("fixtures").join(name));
        assert_eq!(triage(&log, &table).as_deref(), expected, "fixture {name}");
    }

    let clean = "Starting: update-system-pkgs\n:: Synchronizing package databases...\n\
                 Completed: update-system-pkgs\n";
    assert_eq!(triage(clean, &table), None);
}
