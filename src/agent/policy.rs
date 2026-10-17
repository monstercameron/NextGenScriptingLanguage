//! `# execute-policy: allow CATEGORY...` header: the approvals a program's author
//! declares. It is an ordinary leading comment, so the grammar is unchanged. The
//! header is read only by `execute explain --json --policy-header`, which merges it
//! into the gate's approved categories. It does not grant anything at run time.
//!
//! Fail-closed rules: exactly one header is allowed, it must be in the leading
//! comment block, and every category must be a known gate category. `allow none`
//! declares that nothing is approved.

use crate::agent::gate::parse_category;
use anyhow::{bail, Result};
use std::collections::BTreeSet;

const HEADER: &str = "# execute-policy:";

/// The categories a program's leading `# execute-policy:` header approves.
pub fn header_categories(source: &str) -> Result<BTreeSet<String>> {
    let mut headers = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with('#') {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix(HEADER) {
            headers.push(rest.trim().to_owned());
        }
    }
    match headers.as_slice() {
        [] => bail!("no `{HEADER} allow ...` header in the leading comments"),
        [single] => parse_header(single),
        _ => bail!("more than one `{HEADER}` header in the leading comments"),
    }
}

fn parse_header(rest: &str) -> Result<BTreeSet<String>> {
    let mut words = rest.split_whitespace();
    if words.next() != Some("allow") {
        bail!("`{HEADER}` must be `allow CATEGORY...` or `allow none`");
    }
    let names: Vec<&str> = words.collect();
    if names == ["none"] {
        return Ok(BTreeSet::new());
    }
    if names.is_empty() {
        bail!("`{HEADER}` must name at least one category, or `allow none`");
    }
    let mut categories = BTreeSet::new();
    for name in names {
        if name == "none" {
            bail!("`none` cannot be combined with other categories in `{HEADER}`");
        }
        categories.insert(parse_category(name)?.to_owned());
    }
    Ok(categories)
}
