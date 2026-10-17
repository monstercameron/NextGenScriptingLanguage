//! `execute repair CHECKPOINT --op '<operation>' [--dry-run] [--manifest-dir DIR]`.
//! The repair logic lives in the library crate.

use anyhow::{bail, Context, Result};
use flow::repair::{self, Operation};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

const USAGE: &str =
    "usage: execute repair CHECKPOINT --op '<operation>' [--dry-run] [--manifest-dir DIR]\n\
    operations: set-parameter LINE NAME VALUE | replace-command LINE STATEMENT | \
    insert-before LINE STATEMENT | remove LINE | add-retry LINE N";

pub fn repair(arguments: &[String]) -> Result<()> {
    let mut directory = None;
    let mut operation_text = None;
    let mut manifest_dir = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--op" => {
                index += 1;
                let text = arguments.get(index).context("--op requires an operation")?;
                if operation_text.replace(text.clone()).is_some() {
                    bail!("--op may be given only once; {USAGE}");
                }
            }
            "--manifest-dir" => {
                index += 1;
                manifest_dir = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--manifest-dir requires a directory")?,
                ));
            }
            "--dry-run" => dry_run = true,
            option if option.starts_with("--") => {
                bail!("unknown repair option {option:?}; {USAGE}")
            }
            path if directory.is_none() => directory = Some(PathBuf::from(path)),
            _ => bail!("repair takes one checkpoint directory; {USAGE}"),
        }
        index += 1;
    }
    let directory = directory.with_context(|| USAGE.to_owned())?;
    let operation_text = operation_text.with_context(|| USAGE.to_owned())?;
    let operation = Operation::parse(&operation_text)?;

    let (program_path, completed_steps) = repair::repairable_program(&directory)?;
    let source = fs::read_to_string(&program_path)
        .with_context(|| format!("reading {}", program_path.display()))?;
    let registry = crate::load_workflow_registry(manifest_dir, None, None, None)?;
    let boundary = repair::boundary_line(&source, completed_steps)?;
    let plan = repair::apply(&source, &operation, boundary, &registry)?;

    let mut out = io::stdout().lock();
    writeln!(out, "--- {}", program_path.display())?;
    writeln!(out, "+++ {} (repaired)", program_path.display())?;
    write!(out, "{}", plan.diff)?;
    if dry_run {
        writeln!(out, "dry run: nothing was written")?;
        return Ok(());
    }
    let sequence = repair::commit(&directory, &program_path, &plan, &operation_text)?;
    writeln!(
        out,
        "repair {sequence} applied; recorded in {}",
        directory.join(repair::REPAIRS_FILE).display()
    )?;
    writeln!(
        out,
        "next: execute resume {} --checkpoint {}",
        program_path.display(),
        directory.display()
    )?;
    Ok(())
}
