//! `execute run-parts DIR [--once STATE_DIR] [--continue] [--json]`: run the
//! `*.program` files directly inside DIR in lexical order, like `run-parts(8)`.
//!
//! Each program runs in its own `execute` process. The run stops at the first
//! failure unless `--continue` is given. With `--once`, each completed program's name
//! and SHA-256 content hash are recorded in `STATE_DIR/state.json`, so a later run skips
//! it. A program whose content changed since it completed is reported as `changed` and
//! is not run again: the operator decides whether to reset its state. `--json` prints
//! one document; the programs' own output then goes to standard error so standard output
//! stays parseable.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const PROGRAM_SUFFIX: &str = ".program";
const STATE_FILE: &str = "state.json";

#[derive(Default)]
struct Options {
    directory: Option<PathBuf>,
    once: Option<PathBuf>,
    continue_after_failure: bool,
    json: bool,
    manifest_dir: Option<PathBuf>,
    policy: crate::execution_policy_options::ExecutionPolicyOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Ran,
    Skipped,
    Changed,
    Failed(Option<i32>),
    NotRun,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ran => "ran",
            Status::Skipped => "skipped",
            Status::Changed => "changed",
            Status::Failed(_) => "failed",
            Status::NotRun => "not_run",
        }
    }
}

#[derive(Debug)]
struct Entry {
    name: String,
    hash: String,
    status: Status,
}

/// Entry point for `execute run-parts ...`. Exits with status 1 when a program fails.
pub fn cli(arguments: &[String]) -> Result<()> {
    let options = parse(arguments)?;
    options.policy.load()?;
    let directory = options
        .directory
        .clone()
        .context("run-parts requires a directory")?;
    let programs = list_programs(&directory)?;
    let executable = std::env::current_exe().context("locating the execute executable")?;
    let mut state = match &options.once {
        Some(dir) => load_state(&dir.join(STATE_FILE))?,
        None => BTreeMap::new(),
    };

    let mut entries = Vec::new();
    let mut stopped = false;
    for (name, path) in programs {
        let content = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let hash = sha256_hex(&content);
        if stopped {
            entries.push(Entry {
                name,
                hash,
                status: Status::NotRun,
            });
            continue;
        }
        let recorded = state.get(&name).cloned();
        let status = match (&options.once, recorded) {
            (Some(_), Some(previous)) if previous == hash => Status::Skipped,
            (Some(_), Some(_)) => Status::Changed,
            _ => run_program(&executable, &path, &options, &name)?,
        };
        if status == Status::Ran {
            if let Some(dir) = &options.once {
                state.insert(name.clone(), hash.clone());
                save_state(&dir.join(STATE_FILE), &state)?;
            }
        }
        if matches!(status, Status::Failed(_)) && !options.continue_after_failure {
            stopped = true;
        }
        entries.push(Entry { name, hash, status });
    }

    let failed = entries
        .iter()
        .any(|entry| matches!(entry.status, Status::Failed(_)));
    if options.json {
        writeln!(
            io::stdout(),
            "{}",
            serde_json::to_string_pretty(&json_report(&directory, &entries))?
        )?;
    } else {
        for entry in &entries {
            writeln!(
                io::stdout(),
                "run-parts: {} {}",
                entry.name,
                describe(&entry.status)
            )?;
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn parse(arguments: &[String]) -> Result<Options> {
    let mut options = Options::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--once" => {
                index += 1;
                options.once = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--once requires a state directory")?,
                ));
            }
            "--manifest-dir" => {
                index += 1;
                options.manifest_dir = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--manifest-dir requires a directory")?,
                ));
            }
            "--continue" => options.continue_after_failure = true,
            "--json" => options.json = true,
            _ if options.policy.parse_option(arguments, &mut index)? => {}
            option if option.starts_with("--") => bail!("unknown run-parts option {option:?}"),
            directory => {
                if options.directory.is_some() {
                    bail!("run-parts accepts one directory");
                }
                options.directory = Some(PathBuf::from(directory));
            }
        }
        index += 1;
    }
    Ok(options)
}

/// Programs directly inside `directory`, in lexical byte order. Subdirectories and
/// hidden files are not run, like `run-parts`.
pub fn list_programs(directory: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !directory.is_dir() {
        bail!("{} is not a directory", directory.display());
    }
    let mut programs = Vec::new();
    for entry in
        fs::read_dir(directory).with_context(|| format!("reading {}", directory.display()))?
    {
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            bail!("program name is not UTF-8: {}", path.display());
        };
        if name.starts_with('.') || !name.ends_with(PROGRAM_SUFFIX) || !path.is_file() {
            continue;
        }
        programs.push((name, path));
    }
    programs.sort();
    Ok(programs)
}

fn run_program(executable: &Path, path: &Path, options: &Options, name: &str) -> Result<Status> {
    let mut command = Command::new(executable);
    command.arg(path);
    if let Some(manifest_dir) = &options.manifest_dir {
        command.arg("--manifest-dir").arg(manifest_dir);
    }
    options.policy.forward(&mut command);
    let output = command
        .output()
        .with_context(|| format!("starting {}", executable.display()))?;
    // Forward the program's output. In JSON mode it goes to standard error so that
    // standard output carries only the report.
    if options.json {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
    } else {
        io::stdout().write_all(&output.stdout)?;
    }
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    let _ = name;
    if output.status.success() {
        Ok(Status::Ran)
    } else {
        Ok(Status::Failed(output.status.code()))
    }
}

fn describe(status: &Status) -> String {
    match status {
        Status::Ran => "ran".to_owned(),
        Status::Skipped => "skipped (already completed)".to_owned(),
        Status::Changed => "changed since it completed; not run again".to_owned(),
        Status::Failed(Some(code)) => format!("failed (exit {code})"),
        Status::Failed(None) => "failed (killed by a signal)".to_owned(),
        Status::NotRun => "not run (an earlier program failed)".to_owned(),
    }
}

pub fn sha256_hex(content: &[u8]) -> String {
    Sha256::digest(content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn load_state(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write the state through a temporary file and a rename, so an interrupted write
/// cannot leave a truncated record behind.
fn save_state(path: &Path, state: &BTreeMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn json_report(directory: &Path, entries: &[Entry]) -> Value {
    let count = |label: &str| {
        entries
            .iter()
            .filter(|entry| entry.status.label() == label)
            .count()
    };
    json!({
        "protocol": "execute.run-parts/1",
        "directory": directory.display().to_string(),
        "summary": {
            "ran": count("ran"),
            "skipped": count("skipped"),
            "changed": count("changed"),
            "failed": count("failed"),
            "not_run": count("not_run"),
        },
        "results": entries.iter().map(|entry| json!({
            "name": entry.name,
            "status": entry.status.label(),
            "hash": entry.hash,
            "exit": match entry.status {
                Status::Failed(code) => json!(code),
                _ => Value::Null,
            },
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_listed_in_lexical_order_and_hidden_files_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "20-second.program",
            "10-first.program",
            ".hidden.program",
            "notes.txt",
        ] {
            fs::write(directory.path().join(name), "").unwrap();
        }
        fs::create_dir(directory.path().join("30-dir.program")).unwrap();
        let names: Vec<String> = list_programs(directory.path())
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["10-first.program", "20-second.program"]);
    }

    #[test]
    fn state_round_trips_and_hashes_are_sha256() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/state.json");
        let mut state = BTreeMap::new();
        state.insert("a.program".to_owned(), sha256_hex(b"abc"));
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path).unwrap(), state);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn options_require_one_directory_and_known_flags() {
        assert!(parse(&["a".into(), "b".into()]).is_err());
        assert!(parse(&["--bogus".into()]).is_err());
        let options = parse(&[
            "dir".into(),
            "--once".into(),
            "st".into(),
            "--continue".into(),
        ])
        .unwrap();
        assert!(options.continue_after_failure);
        assert_eq!(options.once, Some(PathBuf::from("st")));
    }
}
