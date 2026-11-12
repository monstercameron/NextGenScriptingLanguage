//! `execute repair CHECKPOINT --op '<operation>' [--dry-run] [--manifest-dir DIR]`
//! or `execute repair --from-record DIR --program FILE --op '<operation>' ...`.
//! The repair logic lives in the library crate.

use anyhow::{bail, Context, Result};
use flow::repair::{self, Operation};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const USAGE: &str =
    "usage: execute repair CHECKPOINT --op '<operation>' [--op '<operation>' ...] [--dry-run] [--manifest-dir DIR]\n\
    usage: execute repair --from-record DIR --program FILE --op '<operation>' [--op '<operation>' ...] [--dry-run] [--manifest-dir DIR]\n\
    operations: set-parameter LINE NAME VALUE | replace-command LINE STATEMENT | \
    insert-before LINE STATEMENT | remove LINE | add-retry LINE N \
    (LINE is a physical line, or a step id or label such as fetch; repeated --op values apply in order and the result is checked once)";

pub fn repair(arguments: &[String]) -> Result<()> {
    let mut directory = None;
    let mut from_record = None;
    let mut program_override = None;
    let mut operation_texts: Vec<String> = Vec::new();
    let mut manifest_dir = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--op" => {
                index += 1;
                let text = arguments.get(index).context("--op requires an operation")?;
                operation_texts.push(text.clone());
            }
            "--from-record" => {
                index += 1;
                if from_record.is_some() {
                    bail!("--from-record may be given only once; {USAGE}");
                }
                from_record = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--from-record requires a record directory")?,
                ));
            }
            "--program" => {
                index += 1;
                if program_override.is_some() {
                    bail!("--program may be given only once; {USAGE}");
                }
                program_override = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--program requires a program file")?,
                ));
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
            path if directory.is_none() && from_record.is_none() => {
                directory = Some(PathBuf::from(path))
            }
            _ => bail!("repair takes one checkpoint directory; {USAGE}"),
        }
        index += 1;
    }
    if operation_texts.is_empty() {
        bail!("{USAGE}");
    }
    let operations = operation_texts
        .iter()
        .map(|text| Operation::parse(text))
        .collect::<Result<Vec<_>>>()?;
    // The audit record keeps every operation's text, one per line, in the order applied.
    let operation_text = operation_texts.join("\n");
    let registry = crate::load_workflow_registry(
        manifest_dir,
        None,
        flow::manifest::ManifestCheck::Lazy,
        None,
        None,
    )?;

    if let Some(record) = from_record {
        if directory.is_some() {
            bail!("--from-record cannot be combined with a checkpoint directory; {USAGE}");
        }
        let program_path = program_override.with_context(|| {
            "repair --from-record needs the program file to edit: pass --program FILE"
        })?;
        return repair_from_record(
            &record,
            &program_path,
            &operations,
            &operation_text,
            dry_run,
            &registry,
        );
    }
    if program_override.is_some() {
        bail!("--program applies only to repair --from-record; {USAGE}");
    }
    let directory = directory.with_context(|| USAGE.to_owned())?;

    // Hold the checkpoint lock across the whole read, check and commit, so a
    // live resume, a resolve or another repair cannot interleave with this
    // repair; a held lock fails here with the busy error. A dry run writes
    // nothing, so it takes no lock and leaves the checkpoint untouched.
    let _repair_lock = if dry_run {
        None
    } else {
        Some(repair::acquire_repair_lock(&directory)?)
    };
    let (program_path, completed_steps) = repair::repairable_program(&directory)?;
    let source = fs::read_to_string(&program_path)
        .with_context(|| format!("reading {}", program_path.display()))?;
    let plan = plan_repair(
        &source,
        &program_path,
        completed_steps,
        &operations,
        &registry,
    )?;
    if !dry_run {
        // Still under the lock: refuse a plan whose checkpoint state has moved
        // since it was read, rather than applying a stale plan.
        let (current_path, current_steps) = repair::repairable_program(&directory)?;
        if current_path != program_path || current_steps != completed_steps {
            bail!(
                "the checkpoint {} changed while this repair was planned; nothing was written, run repair again",
                directory.display()
            );
        }
    }
    finish_repair(
        &directory,
        &program_path,
        &plan,
        &operation_text,
        dry_run,
        &format!(
            "next: execute resume {} --checkpoint {}",
            program_path.display(),
            directory.display()
        ),
    )
}

/// Repair the failed suffix of a program run with `--record`, without a
/// checkpoint. The record's first failed step sets the boundary: its
/// top-level statement and everything after it may change, earlier lines may
/// not. The repair is recorded in `repairs.jsonl` beside the record, and the
/// record's own `run.json` and `trace.jsonl` are never modified.
fn repair_from_record(
    record_directory: &Path,
    program_path: &Path,
    operations: &[Operation],
    operation_text: &str,
    dry_run: bool,
    registry: &flow::manifest::Registry,
) -> Result<()> {
    // Hold the record directory's lock across the read, check and commit, so
    // two repairs of the same record cannot both append to repairs.jsonl. A
    // dry run writes nothing and takes no lock.
    let _repair_lock = if dry_run {
        None
    } else {
        Some(repair::acquire_repair_lock(record_directory)?)
    };
    let record = flow::record::StoredRecord::open(record_directory)?;
    if record.run["result"]["ok"].as_bool() == Some(true) {
        bail!("the record completed successfully; there is no failed suffix to repair");
    }
    // A truncated, torn, or provenance-mismatched record cannot bound the
    // repair: refuse it instead of guessing a boundary over completed steps.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    flow::record::check_from_record_usable(&record, registry, &cwd)?;
    let source =
        fs::read(program_path).with_context(|| format!("reading {}", program_path.display()))?;
    if !flow::replay::program_matches_record(&record, &source) {
        // Line numbers in the repair refer to this file, so a program that no
        // longer matches the record cannot be bounded safely. Refusing beats
        // guessing at a shifted boundary over completed steps.
        bail!(
            "the program {} differs from the program the record {} ran; repair the recorded program",
            program_path.display(),
            record_directory.display()
        );
    }
    let text = std::str::from_utf8(&source).context("the program is not UTF-8")?;
    let (boundary, failed_line) = repair::from_record_boundary(text, Some(program_path), &record)?;
    let plan = repair::apply_all_at(text, Some(program_path), operations, boundary, registry)?;
    if let Some(line) = failed_line {
        eprintln!(
            "repair: the record failed at line {line}; lines before {boundary} are completed and unchanged"
        );
    }
    finish_repair(
        record_directory,
        program_path,
        &plan,
        operation_text,
        dry_run,
        &format!(
            "next: execute resume {} --from-record {}",
            program_path.display(),
            record_directory.display()
        ),
    )
}

fn finish_repair(
    directory: &Path,
    program_path: &Path,
    plan: &repair::Plan,
    operation_text: &str,
    dry_run: bool,
    next: &str,
) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "--- {}", program_path.display())?;
    writeln!(out, "+++ {} (repaired)", program_path.display())?;
    write!(out, "{}", plan.diff)?;
    if dry_run {
        writeln!(out, "dry run: nothing was written")?;
        return Ok(());
    }
    // The plan was made from `plan.before`. If the program on disk no longer
    // matches it, the audit record would name a hash the file does not have,
    // so refuse before anything is written.
    repair::ensure_program_unchanged(program_path, plan)?;
    let sequence = repair::commit(directory, program_path, plan, operation_text)?;
    writeln!(
        out,
        "repair {sequence} applied; recorded in {}",
        directory.join(repair::REPAIRS_FILE).display()
    )?;
    writeln!(out, "{next}")?;
    Ok(())
}

fn plan_repair(
    source: &str,
    program_path: &std::path::Path,
    completed_steps: usize,
    operations: &[Operation],
    registry: &flow::manifest::Registry,
) -> Result<repair::Plan> {
    let boundary = repair::boundary_line_at(source, Some(program_path), completed_steps)?;
    repair::apply_all_at(source, Some(program_path), operations, boundary, registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_plan_uses_file_aware_parsing_and_physical_statement_lines() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("lib.program");
        fs::write(&library, "run \"printf\" with arguments [\"lib\"]\n").unwrap();
        let path = directory.path().join("main.program");
        let file_call = "run program file \"lib.program\"\nrun \"false\"\n";
        fs::write(&path, file_call).unwrap();
        let plan = plan_repair(
            file_call,
            &path,
            1,
            &[Operation::parse("replace-command 2 run \"true\"").unwrap()],
            &flow::manifest::Registry::default(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "run program file \"lib.program\"\nrun \"true\"\n"
        );

        let multiline = "run \"printf\" with arguments [\n  \"a\",\n  \"b\"\n]\nrun \"false\"\n";
        fs::write(&path, multiline).unwrap();
        let plan = plan_repair(
            multiline,
            &path,
            1,
            &[Operation::parse("replace-command 5 run \"true\"").unwrap()],
            &flow::manifest::Registry::default(),
        )
        .unwrap();
        assert!(plan.after.ends_with("run \"true\"\n"));

        // The quoted command is a direct executable, so its argv uses `arguments`.
        let raw_block = "use value \"\"\"\nline one \"quoted\" \\back\n# block text\n\"\"\"\npipe to \"wc\" with arguments [\"-c\"]\nrun \"false\"\n";
        fs::write(&path, raw_block).unwrap();
        let plan = plan_repair(
            raw_block,
            &path,
            1,
            &[Operation::parse("replace-command 6 run \"true\"").unwrap()],
            &flow::manifest::Registry::default(),
        )
        .unwrap();
        assert!(plan.after.contains("run \"true\"\n"));
    }

    #[test]
    fn repair_plan_accepts_a_step_label_as_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.program");
        let source = "run \"true\"\nfetch: run \"false\"\nrun \"true\"\n";
        fs::write(&path, source).unwrap();
        // One completed step: the boundary is the labelled step's line, so it may change.
        let plan = plan_repair(
            source,
            &path,
            1,
            &[Operation::parse("remove fetch").unwrap()],
            &flow::manifest::Registry::default(),
        )
        .unwrap();
        assert_eq!(plan.line, 2);
        assert_eq!(plan.after, "run \"true\"\nrun \"true\"\n");
    }

    #[test]
    fn repeated_ops_apply_in_order_and_a_failing_one_refuses_the_batch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.program");
        let source = "fetch: run \"true\"\nrun \"false\"\nrun \"true\"\n";
        fs::write(&path, source).unwrap();
        // The label is looked up in the text the first operation produced, so it
        // still names the fetch step after the insert moved it to line 2.
        let plan = plan_repair(
            source,
            &path,
            0,
            &[
                Operation::parse("insert-before 1 run \"true\"").unwrap(),
                Operation::parse("remove fetch").unwrap(),
            ],
            &flow::manifest::Registry::default(),
        )
        .unwrap();
        assert_eq!(plan.line, 2);
        assert_eq!(plan.after, "run \"true\"\nrun \"false\"\nrun \"true\"\n");

        let error = plan_repair(
            source,
            &path,
            0,
            &[
                Operation::parse("remove 1").unwrap(),
                Operation::parse("remove 9").unwrap(),
            ],
            &flow::manifest::Registry::default(),
        )
        .expect_err("a failing second operation must refuse the batch");
        assert!(format!("{error:#}").contains("past the end"), "{error:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), source);
    }

    #[test]
    fn repair_from_record_refuses_while_the_record_lock_is_held() {
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("record");
        fs::create_dir(&record).unwrap();
        let path = directory.path().join("main.program");
        let source = "run \"false\"\n";
        fs::write(&path, source).unwrap();
        let operation_text = "replace-command 1 run \"true\"";
        let _held = repair::acquire_repair_lock(&record).unwrap();

        let error = repair_from_record(
            &record,
            &path,
            &[Operation::parse(operation_text).unwrap()],
            operation_text,
            false,
            &flow::manifest::Registry::default(),
        )
        .expect_err("a held record lock must refuse the repair");
        assert!(format!("{error:#}").contains("locked"), "{error:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), source);
        assert!(!record.join(repair::REPAIRS_FILE).exists());
    }
}
