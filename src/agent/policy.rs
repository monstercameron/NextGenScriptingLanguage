//! `# execute-policy: allow CATEGORY...` header: the categories a program declares
//! it intends to use. It is an ordinary leading comment, so the grammar is unchanged.
//! The header is read only by `execute explain --json --policy-header`, which reports
//! every effect category the program does not declare as a violation.
//!
//! The header never approves anything. Approvals come only from `--allow`, and a
//! category needs both an approval and a declaration to pass. A header-only check
//! without `--allow` therefore denies every effect. `allow none` declares that the
//! program uses no effects.
//!
//! Fail-closed rules: exactly one header is allowed, it must be in the leading
//! comment block, and every category must be a known gate category.

use crate::agent::gate::parse_category;
use anyhow::{bail, Result};
use std::collections::BTreeSet;

const HEADER: &str = "execute-policy:";

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
        if let Some(rest) = header_rest(trimmed) {
            headers.push(rest.trim().to_owned());
        }
    }
    match headers.as_slice() {
        [] => bail!("no `# {HEADER} allow ...` header in the leading comments"),
        [single] => parse_header(single),
        _ => bail!("more than one `# {HEADER}` header in the leading comments"),
    }
}

/// Enforce a program's declared effect categories before its first runtime effect.
/// A source without the opt-in header keeps legacy behavior and pays only one
/// substring check on the enforcement path.
pub fn enforce_header(
    source: &str,
    program: &crate::ast::Program,
    registry: &crate::manifest::Registry,
    program_path: &str,
) -> Result<()> {
    if !has_leading_header(source) {
        return Ok(());
    }
    let declared = header_categories(source)?;
    let plan = crate::agent::explain::explain_json(program, registry, program_path)?;
    let all_categories = crate::agent::gate::CATEGORIES
        .iter()
        .map(|category| (*category).to_owned())
        .collect();
    let verdict = crate::agent::gate::verdict(&plan, &all_categories, Some(&declared));
    if verdict["allowed"] == true {
        return Ok(());
    }
    let violations = verdict["violations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|violation| {
            format!(
                "line {}: {}",
                violation["line"].as_u64().unwrap_or(0),
                violation["category"].as_str().unwrap_or("undeclared")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    bail!("execute-policy header denies effects: {violations}")
}

fn has_leading_header(source: &str) -> bool {
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with('#') {
            return false;
        }
        if header_rest(trimmed).is_some() {
            return true;
        }
    }
    false
}

fn header_rest(comment: &str) -> Option<&str> {
    comment
        .strip_prefix('#')?
        .trim_start()
        .trim_start_matches('#')
        .trim_start()
        .strip_prefix(HEADER)
}

fn parse_header(rest: &str) -> Result<BTreeSet<String>> {
    let mut words = rest.split_whitespace();
    if words.next() != Some("allow") {
        bail!("`# {HEADER}` must be `allow CATEGORY...` or `allow none`");
    }
    let names: Vec<&str> = words.collect();
    if names == ["none"] {
        return Ok(BTreeSet::new());
    }
    if names.is_empty() {
        bail!("`# {HEADER}` must name at least one category, or `allow none`");
    }
    let mut categories = BTreeSet::new();
    for name in names {
        if name == "none" {
            bail!("`none` cannot be combined with other categories in `# {HEADER}`");
        }
        categories.insert(parse_category(name)?.to_owned());
    }
    Ok(categories)
}

#[cfg(test)]
mod tests {
    use super::{enforce_header, header_categories};

    #[test]
    fn allow_none_refuses_unclassifiable_direct_commands() {
        let source = "### execute-policy: allow none\nrun \"echo\" with arguments [\"x\"]\n";
        let program = crate::parser::parse(source).unwrap();
        let error = enforce_header(
            source,
            &program,
            &crate::manifest::Registry::default(),
            "test.program",
        )
        .unwrap_err();
        assert!(error.to_string().contains("undeclared"));
    }

    #[test]
    fn missing_policy_header_keeps_the_legacy_path() {
        let source = "run \"echo\" with arguments [\"x\"]\n";
        let program = crate::parser::parse(source).unwrap();
        assert!(enforce_header(
            source,
            &program,
            &crate::manifest::Registry::default(),
            "test.program"
        )
        .is_ok());
        assert!(header_categories(source).is_err());
    }
}
