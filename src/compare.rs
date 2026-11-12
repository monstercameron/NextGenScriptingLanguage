//! Comparison of two captured command outcomes (issue #403).
//!
//! This module is pure: it takes already-captured outcomes and reports where they differ. It
//! starts no process and reads no file. The CLI wiring that captures outcomes is not here.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Everything observed from one run: exit status, output, fake-binary call log and the final
/// file tree, keyed by a path the caller has already normalised.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub fake_calls: Vec<String>,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// One place where the original and the port disagree.
///
/// `field` is one of `status`, `stdout`, `stderr`, `fake_calls` or `files`. For `files`,
/// `path` names the file. `detail` describes the difference without copying raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub field: &'static str,
    pub path: Option<String>,
    pub detail: String,
}

/// The result of comparing an original run with a port run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub differences: Vec<Difference>,
}

impl Report {
    /// True when no field differs.
    pub fn is_identical(&self) -> bool {
        self.differences.is_empty()
    }

    /// The report as a JSON document: `{"identical": bool, "differences": [...]}`.
    pub fn to_json(&self) -> Value {
        let differences: Vec<Value> = self
            .differences
            .iter()
            .map(|difference| {
                json!({
                    "field": difference.field,
                    "path": difference.path,
                    "detail": difference.detail,
                })
            })
            .collect();
        json!({
            "identical": self.is_identical(),
            "differences": differences,
        })
    }
}

/// Compares an original outcome with a port outcome, field by field.
pub fn compare(original: &Outcome, port: &Outcome) -> Report {
    let mut differences = Vec::new();

    if original.status != port.status {
        differences.push(Difference {
            field: "status",
            path: None,
            detail: format!("original {:?}, port {:?}", original.status, port.status),
        });
    }

    if let Some(detail) = bytes_difference(&original.stdout, &port.stdout) {
        differences.push(Difference {
            field: "stdout",
            path: None,
            detail,
        });
    }

    if let Some(detail) = bytes_difference(&original.stderr, &port.stderr) {
        differences.push(Difference {
            field: "stderr",
            path: None,
            detail,
        });
    }

    if let Some(detail) = calls_difference(&original.fake_calls, &port.fake_calls) {
        differences.push(Difference {
            field: "fake_calls",
            path: None,
            detail,
        });
    }

    // BTreeMap keys are sorted, so the union below is reported in a stable order.
    let mut paths: Vec<&String> = original.files.keys().chain(port.files.keys()).collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        let detail = match (original.files.get(path), port.files.get(path)) {
            (Some(_), None) => Some("present in original, missing in port".to_string()),
            (None, Some(_)) => Some("missing in original, present in port".to_string()),
            (Some(left), Some(right)) => bytes_difference(left, right),
            (None, None) => None,
        };
        if let Some(detail) = detail {
            differences.push(Difference {
                field: "files",
                path: Some(path.clone()),
                detail,
            });
        }
    }

    Report { differences }
}

/// Describes the first byte at which two byte strings differ, if any.
fn bytes_difference(original: &[u8], port: &[u8]) -> Option<String> {
    let offset = original
        .iter()
        .zip(port)
        .position(|(left, right)| left != right)
        .or_else(|| (original.len() != port.len()).then(|| original.len().min(port.len())))?;
    Some(format!(
        "first difference at byte {offset}; original {} bytes, port {} bytes",
        original.len(),
        port.len()
    ))
}

/// Describes the first fake-call entry at which two call logs differ, if any.
fn calls_difference(original: &[String], port: &[String]) -> Option<String> {
    let index = original
        .iter()
        .zip(port)
        .position(|(left, right)| left != right)
        .or_else(|| (original.len() != port.len()).then(|| original.len().min(port.len())))?;
    Some(format!(
        "first difference at call {index}; original {} calls, port {} calls",
        original.len(),
        port.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Outcome {
        let mut files = BTreeMap::new();
        files.insert("out/result.txt".to_string(), b"done\n".to_vec());
        Outcome {
            status: Some(0),
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            fake_calls: vec!["git status".to_string()],
            files,
        }
    }

    #[test]
    fn identical_outcomes_have_no_differences() {
        let report = compare(&sample(), &sample());
        assert!(report.is_identical());
        assert_eq!(report.to_json()["identical"], json!(true));
    }

    #[test]
    fn one_changed_stdout_byte_names_stdout() {
        let original = sample();
        let mut port = sample();
        port.stdout = b"hellO\n".to_vec();
        let report = compare(&original, &port);
        assert_eq!(report.differences.len(), 1);
        assert_eq!(report.differences[0].field, "stdout");
        assert_eq!(report.differences[0].path, None);
        assert!(report.differences[0].detail.contains("byte 4"));
    }

    #[test]
    fn stdout_length_difference_reports_the_shorter_length_as_offset() {
        let original = sample();
        let mut port = sample();
        port.stdout = b"hello".to_vec();
        let report = compare(&original, &port);
        assert_eq!(report.differences.len(), 1);
        assert!(report.differences[0].detail.contains("byte 5"));
    }

    #[test]
    fn status_and_call_log_differences_are_reported_in_order() {
        let original = sample();
        let mut port = sample();
        port.status = Some(2);
        port.fake_calls.push("rm -rf out".to_string());
        let report = compare(&original, &port);
        let fields: Vec<&str> = report.differences.iter().map(|d| d.field).collect();
        assert_eq!(fields, vec!["status", "fake_calls"]);
    }

    #[test]
    fn missing_and_changed_files_name_their_path() {
        let original = sample();
        let mut port = sample();
        port.files.clear();
        port.files
            .insert("out/extra.txt".to_string(), b"x".to_vec());
        let report = compare(&original, &port);
        let paths: Vec<Option<&str>> = report
            .differences
            .iter()
            .map(|d| d.path.as_deref())
            .collect();
        assert_eq!(paths, vec![Some("out/extra.txt"), Some("out/result.txt")]);
    }

    #[test]
    fn report_json_names_the_field() {
        let original = sample();
        let mut port = sample();
        port.stderr = b"oops".to_vec();
        let value = compare(&original, &port).to_json();
        assert_eq!(value["identical"], json!(false));
        assert_eq!(value["differences"][0]["field"], json!("stderr"));
        assert_eq!(value["differences"][0]["path"], Value::Null);
    }
}
