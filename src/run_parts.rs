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
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROGRAM_SUFFIX: &str = ".program";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = "state.lock";

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
        let status = match &options.once {
            Some(dir) => {
                // The lock covers the whole read, run and write of this program. An
                // overlapping invocation waits here, then reads the completion record
                // and skips the program instead of running it a second time.
                let _lock = StateLock::acquire(dir)?;
                let state_path = dir.join(STATE_FILE);
                let recorded = load_state(&state_path)?.get(&name).cloned();
                match recorded {
                    Some(previous) if previous == hash => Status::Skipped,
                    Some(_) => Status::Changed,
                    None => {
                        let status = run_program(&executable, &path, &options, &name)?;
                        if status == Status::Ran {
                            record_completion(&state_path, &name, &hash)?;
                        }
                        status
                    }
                }
            }
            None => run_program(&executable, &path, &options, &name)?,
        };
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
    // The program's output is copied through as it is written, so a long program shows
    // progress and holds no more than one chunk per stream in memory. In JSON mode it
    // goes to standard error so that standard output carries only the report.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", executable.display()))?;
    let child_stdout = child
        .stdout
        .take()
        .context("capturing the program's standard output")?;
    let child_stderr = child
        .stderr
        .take()
        .context("capturing the program's standard error")?;
    let json = options.json;
    let (stdout_result, stderr_result, status) = std::thread::scope(|scope| {
        let stdout_copy = scope.spawn(move || {
            if json {
                forward(child_stdout, &mut io::stderr().lock())
            } else {
                forward(child_stdout, &mut io::stdout().lock())
            }
        });
        let stderr_copy = scope.spawn(move || forward(child_stderr, &mut io::stderr().lock()));
        let status = child.wait();
        (stdout_copy.join(), stderr_copy.join(), status)
    });
    let status = status.with_context(|| format!("waiting for {name}"))?;
    stdout_result.map_err(|_| anyhow::anyhow!("copying the output of {name} panicked"))??;
    stderr_result.map_err(|_| anyhow::anyhow!("copying the output of {name} panicked"))??;
    if status.success() {
        Ok(Status::Ran)
    } else {
        Ok(Status::Failed(status.code()))
    }
}

/// Copy `from` to `to` as the bytes arrive, flushing after each chunk. If a write fails,
/// the rest of the stream is still read and discarded so the child never blocks on a full
/// pipe, and the first write error is returned.
fn forward(mut from: impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0u8; 8192];
    let mut failure: Option<io::Error> = None;
    loop {
        let count = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if failure.is_none() {
                    failure = Some(error);
                }
                break;
            }
        };
        if failure.is_none() {
            if let Err(error) = to.write_all(&buffer[..count]).and_then(|()| to.flush()) {
                failure = Some(error);
            }
        }
    }
    failure.map_or(Ok(()), Err)
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

/// An exclusive lock on `STATE_DIR/state.lock`, held until dropped. It blocks while
/// another `--once` invocation holds it.
struct StateLock {
    _file: File,
}

impl StateLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let path = state_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

/// Record a completed program in the state file. The caller holds the `StateLock`; the
/// file is re-read first so records that another run wrote meanwhile are kept.
fn record_completion(state_path: &Path, name: &str, hash: &str) -> Result<()> {
    let mut state = load_state(state_path)?;
    state.insert(name.to_owned(), hash.to_owned());
    save_state(state_path, &state)
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

    #[test]
    fn state_lock_blocks_a_second_holder_until_the_first_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = directory.path().join("state");
        let holder = StateLock::acquire(&state_dir).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiter_dir = state_dir.clone();
        let waiter = std::thread::spawn(move || {
            let _lock = StateLock::acquire(&waiter_dir).unwrap();
            sender.send(()).unwrap();
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "a second lock was granted while the first was still held"
        );
        drop(holder);
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the waiting lock was never granted");
        waiter.join().unwrap();
    }

    #[test]
    fn a_completion_is_recorded_on_top_of_what_another_run_wrote() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join(STATE_FILE);
        // This run loaded an empty state; another run then recorded "a" before the save.
        let mut other_run = BTreeMap::new();
        other_run.insert("a.program".to_owned(), sha256_hex(b"a"));
        save_state(&state_path, &other_run).unwrap();
        record_completion(&state_path, "b.program", &sha256_hex(b"b")).unwrap();
        let state = load_state(&state_path).unwrap();
        assert_eq!(state.get("a.program"), Some(&sha256_hex(b"a")));
        assert_eq!(state.get("b.program"), Some(&sha256_hex(b"b")));
    }

    /// Writer that records everything written so far in a shared buffer.
    struct Recorder(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl Write for Recorder {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Reader that yields one chunk per call and notes how much output had already
    /// reached the writer when each read happened.
    struct Chunks {
        pending: std::collections::VecDeque<Vec<u8>>,
        written: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        written_at_each_read: Vec<usize>,
    }

    impl Read for Chunks {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.written_at_each_read.push(self.written.borrow().len());
            match self.pending.pop_front() {
                Some(chunk) => {
                    buffer[..chunk.len()].copy_from_slice(&chunk);
                    Ok(chunk.len())
                }
                None => Ok(0),
            }
        }
    }

    #[test]
    fn program_output_is_forwarded_as_it_arrives() {
        let written = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut reader = Chunks {
            pending: vec![b"first\n".to_vec(), b"second\n".to_vec()].into(),
            written: written.clone(),
            written_at_each_read: Vec::new(),
        };
        let mut writer = Recorder(written.clone());
        forward(&mut reader, &mut writer).unwrap();
        assert_eq!(written.borrow().as_slice(), b"first\nsecond\n");
        // Each chunk reached the writer before the next read, so nothing waited for exit.
        assert_eq!(reader.written_at_each_read, vec![0, 6, 13]);
    }

    #[test]
    fn a_failed_write_still_drains_the_program_and_is_reported() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let written = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut reader = Chunks {
            pending: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()].into(),
            written,
            written_at_each_read: Vec::new(),
        };
        let error = forward(&mut reader, &mut Broken).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(reader.pending.is_empty(), "the program was not drained");
    }
}
