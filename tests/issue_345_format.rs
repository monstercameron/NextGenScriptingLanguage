//! Corpus checks for `flow::format::format_program` (#345). Every `.program` under
//! `examples/` must format, the output must parse again, and formatting must be
//! idempotent. `format_program` itself refuses any output whose program differs from the
//! input (line numbers and generated call labels ignored), so a successful format is also
//! an equivalence check.

use flow::format::FormatError;
use std::path::{Path, PathBuf};

/// Examples the parser rejects before any formatting. Each entry names the parser error
/// seen at #345 time. The test fails when an entry starts parsing, so this list cannot go
/// stale, and fails when a new example stops parsing.
const KNOWN_UNPARSEABLE: &[&str] = &[
    // line 2: operation name must contain words, not quoted values
    "github-stars.program",
    // line 4: operation name must contain words, not quoted values
    "live/widget-console.program",
    // line 5: operation name must contain words, not quoted values
    "stream-file.program",
    // line 2: operation name must contain words, not quoted values
    "top-cpu.program",
    // line 2: unexpected indentation outside a trigger body
    "composition/graph-runtime/graph.program",
    // line 2: unexpected indentation outside a trigger body
    "graph/composition.program",
    // line 7: expected `take <text|integer|unsigned integer|usize|boolean|list of text|json> at <quoted pointer> from <value> as <name>`
    "ladder/127-tail-deployment-jsonl.program",
    // line 2: unexpected indentation outside a trigger body
    "modules/queue_monitor/queue_monitor.graph.program",
    // line 5: expected `take <text|integer|unsigned integer|usize|boolean|list of text|json> at <quoted pointer> from <value> as <name>`
    "stream-events-macos.program",
    // line 4: expected `take <text|integer|unsigned integer|usize|boolean|list of text|json> at <quoted pointer> from <value> as <name>`
    "stream-three-lines.program",
];

/// How many meaning-change failures get a diagnostic in the report.
const DIAGNOSED_FAILURES: usize = 3;

fn collect_programs(directory: &Path, found: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(directory).expect("read examples directory");
    for entry in entries {
        let path = entry.expect("read examples entry").path();
        if path.is_dir() {
            collect_programs(&path, found);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "program")
        {
            found.push(path);
        }
    }
}

#[test]
fn every_example_formats_to_an_equivalent_idempotent_program() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut programs = Vec::new();
    collect_programs(&root, &mut programs);
    assert!(
        !programs.is_empty(),
        "no .program files found under examples/"
    );

    let mut failures = Vec::new();
    let mut diagnosed = 0;
    for path in &programs {
        let source = std::fs::read_to_string(path).expect("read example program");
        let relative = path
            .strip_prefix(&root)
            .expect("example under root")
            .to_string_lossy()
            .replace('\\', "/");
        let parse_error = flow::parser::parse(&source)
            .err()
            .map(|error| format!("{error:#}"));
        let known = KNOWN_UNPARSEABLE.contains(&relative.as_str());
        match (parse_error, known) {
            (None, false) => {}
            (None, true) => failures.push(format!(
                "{}: now parses; remove it from KNOWN_UNPARSEABLE",
                path.display()
            )),
            (Some(_), true) => continue,
            (Some(error), false) => {
                failures.push(format!("{}: does not parse: {error}", path.display()));
                continue;
            }
        }

        let once = match flow::format::format_program(&source) {
            Ok(text) => text,
            Err(error) => {
                let mut message = format!("{}: {error}", path.display());
                if matches!(error, FormatError::MeaningChanged) && diagnosed < DIAGNOSED_FAILURES {
                    diagnosed += 1;
                    if let Some(difference) = flow::format::first_meaning_difference(&source) {
                        message.push_str(" | ");
                        message.push_str(&difference);
                    }
                }
                failures.push(message);
                continue;
            }
        };
        match flow::format::format_program(&once) {
            Ok(twice) if twice == once => {}
            Ok(_) => failures.push(format!("{}: formatting is not idempotent", path.display())),
            Err(error) => failures.push(format!(
                "{}: formatted output does not format again: {error}",
                path.display()
            )),
        }
    }
    // Every report line starts with the example's path, so each line is self-describing.
    assert!(
        failures.is_empty(),
        "{}\n{} of {} examples did not pass",
        failures.join("\n"),
        failures.len(),
        programs.len()
    );
}
