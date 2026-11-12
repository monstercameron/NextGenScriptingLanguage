//! Keeps docs/agents/measurement.md honest. Its table lists the ten ported Omarchy programs
//! under examples/omarchy-ports. Each row must equal what the parser and the static checker
//! report for that file today, and every listed program must pass the check.

use std::fs;
use std::path::{Path, PathBuf};

use flow::manifest::Registry;

const PORTS_DIR: &str = "examples/omarchy-ports";
const MEASUREMENT: &str = "docs/agents/measurement.md";
const ROW_PREFIX: &str = "| examples/omarchy-ports/";

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// The `.program` files in the ports directory, as repository-relative paths in name order.
fn port_programs() -> Vec<String> {
    let directory = repo_path(PORTS_DIR);
    let mut names: Vec<String> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "program")
        })
        .map(|path| {
            let name = path
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            format!("{PORTS_DIR}/{name}")
        })
        .collect();
    names.sort();
    names
}

/// One measured row: what the parser and the static checker report for a program.
struct Row {
    program: String,
    parses: bool,
    diagnostics: usize,
    errors: usize,
    codes: Vec<String>,
}

impl Row {
    fn render(&self) -> String {
        let codes = if self.codes.is_empty() {
            "none".to_string()
        } else {
            self.codes.join(", ")
        };
        format!(
            "| {} | {} | {} | {} | {} |",
            self.program,
            if self.parses { "yes" } else { "no" },
            self.diagnostics,
            self.errors,
            codes
        )
    }
}

/// Measures one program the way `check` analyzes it: the same static analysis over the source,
/// with the file path passed so that called files resolve. The registry is empty, because the
/// default manifest directory is refused by the trust gate on the machine this was written on.
fn measure(program: &str, registry: &Registry) -> Row {
    let path = repo_path(program);
    let source =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let parses = flow::parser::parse(&source).is_ok();
    let diagnostics = flow::diagnostics::analyze_source_at(&source, Some(&path), registry);
    let values: Vec<serde_json::Value> = diagnostics
        .iter()
        .map(|diagnostic| serde_json::to_value(diagnostic).expect("diagnostic serializes"))
        .collect();
    let mut codes: Vec<String> = values
        .iter()
        .map(|value| value["code"].as_str().unwrap_or_default().to_string())
        .collect();
    codes.sort();
    codes.dedup();
    let errors = values
        .iter()
        .filter(|value| value["severity"] == "error")
        .count();
    Row {
        program: program.to_string(),
        parses,
        diagnostics: values.len(),
        errors,
        codes,
    }
}

fn measured_rows() -> Vec<Row> {
    let registry = Registry::default();
    port_programs()
        .iter()
        .map(|program| measure(program, &registry))
        .collect()
}

/// The rows of the committed table, as they are written in the document.
fn committed_rows() -> Vec<String> {
    let text = fs::read_to_string(repo_path(MEASUREMENT))
        .unwrap_or_else(|error| panic!("{MEASUREMENT}: {error}"));
    text.lines()
        .filter(|line| line.starts_with(ROW_PREFIX))
        .map(|line| line.trim_end().to_string())
        .collect()
}

#[test]
fn table_lists_the_ten_ported_programs() {
    let committed = committed_rows();
    let listed: Vec<String> = committed
        .iter()
        .map(|row| {
            row.split('|')
                .nth(1)
                .expect("program cell")
                .trim()
                .to_string()
        })
        .collect();
    assert_eq!(listed.len(), 10, "the measurement table must have ten rows");
    assert_eq!(
        listed,
        port_programs(),
        "the table rows must be exactly the .program files in {PORTS_DIR}, in name order"
    );
}

#[test]
fn table_matches_what_the_checker_reports() {
    let regenerated: Vec<String> = measured_rows().iter().map(Row::render).collect();
    assert_eq!(
        committed_rows(),
        regenerated,
        "{MEASUREMENT} is out of date. Replace its table rows with:\n{}",
        regenerated.join("\n")
    );
}

#[test]
fn every_listed_program_passes_check() {
    let failures: Vec<String> = measured_rows()
        .iter()
        .filter(|row| !row.parses || row.errors > 0)
        .map(Row::render)
        .collect();
    assert!(
        failures.is_empty(),
        "listed programs with errors or parse failures:\n{}",
        failures.join("\n")
    );
}
