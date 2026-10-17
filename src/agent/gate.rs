//! `execute explain FILE --json --allow CATEGORY...`: an effects approval gate.
//!
//! The gate reads the explain plan and reports every row that triggers a category
//! the operator has not approved. It denies by default: with no `--allow`, any
//! effect at all is a violation. With `--policy-header`, a category the program's
//! header does not declare is also a violation. The header restricts and never
//! approves. A retried step reports its attempt count, and `even if uncertain` on a
//! network or writes step also needs `--allow undeclared`.
//!
//! Effect mapping. Each declared effect string maps to one category:
//! `fs.read*` to `reads`; `fs.write*`, `file.write*`, or any name containing
//! `.write` to `writes`; `network.*` to `network`; `process.spawn` to `process`; package installs and
//! service control to `system`.
//! Any other effect is `undeclared`, so an unknown effect needs explicit approval.
//! An invocation whose effects are not stated (null) is `undeclared` too.
//!
//! The verdict is advisory. It does not stop a later run. Enforcement remains the
//! capability policy.

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
    "process",
    "cwd",
    "environment",
    "privileged",
    "system",
    "undeclared",
];

/// Step kinds that run a command. Their effects must be stated, or they are undeclared.
const INVOCATION_KINDS: &[&str] = &[
    "task_group",
    "spawn_job",
    "if_command",
    "while_command",
    "invoke",
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

/// Shipped effect names that the prefix rules below do not place. Each one is mapped on
/// purpose: reading host state is `reads`; creating or locking a file is `writes`; an
/// action that starts or drives another program is `process`. Installing packages and
/// controlling services change the system, so they are `system`, which approving
/// `process` does not cover.
const EXPLICIT_EFFECTS: &[(&str, &str)] = &[
    ("clock.read", "reads"),
    ("compositor.dispatch", "process"),
    ("filesystem.lock", "writes"),
    ("network.loopback", "network"),
    ("package.install", "system"),
    ("systemd.control", "system"),
];

/// Shipped effects that stay `undeclared` on purpose. They change the desktop, and no
/// category describes them better, so using one needs an explicit approval of
/// `undeclared`. Test `shipped_manifest_effects_are_all_placed` keeps this list exact.
pub const DELIBERATELY_UNDECLARED: &[&str] = &["desktop.notify", "desktop.osd", "desktop.render"];

/// The category one declared effect string belongs to.
pub fn effect_category(effect: &str) -> &'static str {
    if let Some((_, category)) = EXPLICIT_EFFECTS.iter().find(|(name, _)| *name == effect) {
        category
    } else if effect.starts_with("fs.read") {
        "reads"
    } else if effect.starts_with("fs.write")
        || effect.starts_with("file.write")
        || effect.contains(".write")
    {
        "writes"
    } else if effect.starts_with("network.") {
        "network"
    } else if effect == "process.spawn" {
        "process"
    } else {
        "undeclared"
    }
}

/// The categories one plan row triggers.
fn triggered(row: &Value) -> Vec<&'static str> {
    // A resolved call row has no effects of its own. Its body is walked as rows.
    if row["kind"] == "call_program" && row["resolved"] == true {
        return Vec::new();
    }
    let mut categories = BTreeSet::new();
    match row["effects"].as_array() {
        Some(effects) => {
            for effect in effects.iter().filter_map(Value::as_str) {
                categories.insert(effect_category(effect));
            }
        }
        None => {
            let invocation = row["kind"]
                .as_str()
                .is_some_and(|kind| INVOCATION_KINDS.contains(&kind));
            if invocation {
                categories.insert("undeclared");
            }
        }
    }
    if row["writes"] == true || row["escapes_cwd"] == true {
        categories.insert("writes");
    }
    if row["network"] == true {
        categories.insert("network");
    }
    if row["reads"] == true {
        categories.insert("reads");
    }
    if row["cwd_change"] == true || row["escapes_cwd"] == true {
        categories.insert("cwd");
    }
    if row["modifies_next_invocation"] == true || row["environment_tainted"] == true {
        categories.insert("environment");
    }
    if row["privileged"] == true {
        categories.insert("privileged");
    }
    if row["undeclared"] == true {
        categories.insert("undeclared");
    }
    // A retry past an uncertain outcome may repeat a network or write effect that
    // already happened, so it needs the explicit `undeclared` approval as well.
    if row["retry"]["even_if_uncertain"] == true
        && (categories.contains("network") || categories.contains("writes"))
    {
        categories.insert("undeclared");
    }
    categories.into_iter().collect()
}

/// Build the verdict from an explain object. A category is a violation when it is
/// not approved by `--allow`, or, when `declared` is given (a program's policy
/// header), when the program does not declare it. The header only restricts: it
/// never approves anything. A step with a retry reports its attempts, because each
/// attempt may repeat its effects.
pub fn verdict(
    explain: &Value,
    allowed: &BTreeSet<String>,
    declared: Option<&BTreeSet<String>>,
) -> Value {
    let mut violations = Vec::new();
    let mut retries = Vec::new();
    for row in explain["steps"].as_array().into_iter().flatten() {
        let attempts = row["retry"]["attempts"].as_u64();
        if let Some(attempts) = attempts {
            retries.push(json!({
                "line": row["line"],
                "kind": row["kind"],
                "attempts": attempts,
                "even_if_uncertain": row["retry"]["even_if_uncertain"],
            }));
        }
        for category in triggered(row) {
            let reason = if !allowed.contains(category) {
                "not_approved"
            } else if declared.is_some_and(|declared| !declared.contains(category)) {
                "not_declared"
            } else {
                continue;
            };
            let mut violation = json!({
                "line": row["line"],
                "kind": row["kind"],
                "category": category,
                "reason": reason,
                "applet": row["applet"],
                "binary": row["binary"],
            });
            if let Some(attempts) = attempts {
                violation["attempts"] = json!(attempts);
            }
            violations.push(violation);
        }
    }
    json!({
        "protocol": PROTOCOL,
        "program": explain["program"],
        "allowed_categories": allowed.iter().collect::<Vec<_>>(),
        "declared_categories": declared.map(|declared| declared.iter().collect::<Vec<_>>()),
        "retries": retries,
        "allowed": violations.is_empty(),
        "violations": violations,
    })
}
