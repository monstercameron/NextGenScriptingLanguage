//! #237: `check` validates a `then` edge against the producer's declared output and the
//! consumer's typed stdin, the same way it already validates `pipe to` edges.

use flow::{diagnostics::analyze_source, manifest::Registry};
use serde_json::json;
use std::{fs, path::Path};

fn registry() -> Registry {
    let producer = json!({
        "name":"fixture.record",
        "binary":"fixture",
        "arguments":[],
        "contract":{"version":1,"output":{
            "type":"record","encoding":"json",
            "fields":{"count":{"type":"integer"}},"required":["count"]
        }}
    });
    let text_sink = json!({
        "name":"fixture.text_sink",
        "binary":"/bin/cat",
        "accepts_stdin":true,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"ports":{"inputs":{
            "stdin":{"schema":{"type":"text"},"delivery":"single","encoding":"raw"}
        }}}
    });
    let record_sink = json!({
        "name":"fixture.record_sink",
        "binary":"/bin/cat",
        "accepts_stdin":true,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"ports":{"inputs":{
            "stdin":{
                "schema":{
                    "type":"record",
                    "fields":{"count":{"type":"integer"}},"required":["count"]
                },
                "delivery":"single","encoding":"json"
            }
        }}}
    });
    Registry::from_sources(&[
        producer.to_string(),
        text_sink.to_string(),
        record_sink.to_string(),
    ])
    .unwrap()
}

fn then_mismatches(source: &str, registry: &Registry) -> Vec<flow::diagnostics::Diagnostic> {
    analyze_source(source, registry)
        .into_iter()
        .filter(|diagnostic| diagnostic.code == "applet.then.type_mismatch")
        .collect()
}

#[test]
fn then_edge_rejects_a_typed_record_into_a_text_stdin_at_the_consumer_step() {
    let registry = registry();
    let source = "run fixture.record\nthen fixture.text_sink\n";

    let mismatches = then_mismatches(source, &registry);
    assert_eq!(mismatches.len(), 1, "{mismatches:?}");
    let span = &mismatches[0].span;
    assert_eq!(
        source[span.start_byte..span.end_byte].trim_end(),
        "then fixture.text_sink"
    );
}

#[test]
fn then_edge_accepts_a_matching_typed_record_unchanged() {
    let registry = registry();
    let source = "run fixture.record\nthen fixture.record_sink\n";

    assert!(then_mismatches(source, &registry).is_empty());
    assert!(analyze_source(source, &registry)
        .iter()
        .all(|diagnostic| diagnostic.code != "applet.pipeline.incompatible"));
}

#[test]
fn shipped_examples_have_no_then_type_mismatch_against_builtin_applets() {
    fn collect(directory: &Path, programs: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, programs);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "program")
            {
                programs.push(path);
            }
        }
    }

    // Only builtin applets are resolvable without the repository's manifests folder, so
    // edges to external applets are skipped here. The coordinator runs the full corpus
    // against the real manifests in a clean export.
    let registry = Registry::default();
    let mut programs = Vec::new();
    collect(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples"),
        &mut programs,
    );
    assert!(!programs.is_empty(), "no example programs were found");

    let mut offenders = Vec::new();
    for program in &programs {
        let Ok(source) = fs::read_to_string(program) else {
            continue;
        };
        if !then_mismatches(&source, &registry).is_empty() {
            offenders.push(program.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "then edges rejected by check: {offenders:#?}"
    );
}
