//! `execute explain FILE --json --allow CATEGORY...`: an effects approval gate.
//!
//! The gate reads the explain plan and reports every row that triggers a category
//! the operator has not approved. It denies by default: with no `--allow`, any
//! effect at all is a violation. An undeclared row is a violation under its own
//! category, so an unknown step cannot pass as harmless. The verdict is advisory.
//! It does not stop a later run. Enforcement remains the capability policy.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Protocol identifier of the verdict object.
pub const PROTOCOL: &str = "execute.gate/1";

/// Categories an operator may approve, in report order.
pub const CATEGORIES: &[&str] = &[
    "writes",
    "network",
    "reads",
    "cwd",
    "environment",
    "privileged",
    "undeclared",
];

/// Validate one `--allow` value. Only the listed categories are accepted.
pub fn parse_category(name: &str) -> Result<&'static str> {
    match CATEGORIES.iter().find(|known| **known == name) {
        Some(category) => Ok(category),
        None => bail!(
            "unknown --allow category {name:?}; expected one of {}",
            CATEGORIES.join(", ")
        ),
    }
}

/// The categories one plan row triggers. A null effect field means the effect is
/// not stated for this row, and an unstated effect is `undeclared`, which is its
/// own category. A resolved call row has no effects of its own; its body is
/// walked as separate rows, so it triggers nothing itself.
fn triggered(row: &Value) -> Vec<&'static str> {
    if row["kind"] == "call_program" && row["resolved"] == true {
        return Vec::new();
    }
    let mut categories = Vec::new();
    if row["writes"] == true || row["escapes_cwd"] == true {
        categories.push("writes");
    }
    if row["network"] == true {
        categories.push("network");
    }
    if row["reads"] == true {
        categories.push("reads");
    }
    if row["cwd_change"] == true || row["escapes_cwd"] == true {
        categories.push("cwd");
    }
    if row["modifies_next_invocation"] == true || row["environment_tainted"] == true {
        categories.push("environment");
    }
    if row["privileged"] == true {
        categories.push("privileged");
    }
    if row["undeclared"] == true {
        categories.push("undeclared");
    }
    categories
}

/// Build the verdict from an explain object and the approved categories.
pub fn verdict(explain: &Value, allowed: &BTreeSet<String>) -> Value {
    let mut violations = Vec::new();
    for row in explain["steps"].as_array().into_iter().flatten() {
        for category in triggered(row) {
            if !allowed.contains(category) {
                violations.push(json!({
                    "line": row["line"],
                    "kind": row["kind"],
                    "category": category,
                    "applet": row["applet"],
                    "binary": row["binary"],
                }));
            }
        }
    }
    json!({
        "protocol": PROTOCOL,
        "program": explain["program"],
        "allowed_categories": allowed.iter().collect::<Vec<_>>(),
        "allowed": violations.is_empty(),
        "violations": violations,
    })
}
