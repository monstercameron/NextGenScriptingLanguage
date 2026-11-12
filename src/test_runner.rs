//! `execute test [PATH...]`: run `*.test.program` files and check their results.
//!
//! A test is `NAME.test.program` with an optional sibling `NAME.test.json`:
//!
//! ```json
//! {"inputs": {"name": "world"}, "arguments": [],
//!  "expect": {"status": 0, "stdout": "hello world", "files": {"out.txt": "hi"}}}
//! ```
//!
//! Each test runs `execute` in a fresh temporary working directory. A `fixtures/bin`
//! directory next to the test is prepended to `PATH`, so fake tools live beside the
//! tests. `EXECUTE_TEST_FIXTURES` names the `fixtures` directory. `stdout` and `stderr`
//! are exact byte comparisons; `stderr_contains` lists substrings that stderr must each
//! contain. `files` maps paths relative to the working directory to their exact
//! contents. `now` pins the injectable clock the tested program reads through
//! `time.now` to a fixed Unix epoch second; `execute test --now EPOCH` pins every
//! test that sets no `now` of its own. `--filter NAME` runs only the test named NAME
//! (the file name without `.test.program`); the others are reported as skipped. Output
//! is TAP 13 by default, and `--json` prints one document for agents. The exit status
//! is 0 only when every test passes.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const PROGRAM_SUFFIX: &str = ".test.program";
const MAX_DISCOVERY_DEPTH: usize = 32;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sidecar {
    pub inputs: BTreeMap<String, String>,
    pub arguments: Vec<String>,
    /// Fixed Unix epoch second for the injectable clock this test's program
    /// reads through `time.now`. Overrides `execute test --now` for this test.
    pub now: Option<i64>,
    pub expect: Expect,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    #[serde(default)]
    pub status: i32,
    #[serde(default)]
    pub stdout: Option<String>,
    /// Exact stderr, compared the same way as `stdout`.
    #[serde(default)]
    pub stderr: Option<String>,
    /// Substrings that stderr must each contain.
    #[serde(default)]
    pub stderr_contains: Vec<String>,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

pub struct Outcome {
    pub name: String,
    pub file: PathBuf,
    pub failures: Vec<String>,
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Entry point for `execute test ...`. Exits with status 1 when any test fails.
pub fn cli(arguments: &[String]) -> Result<()> {
    let mut policy = crate::execution_policy_options::ExecutionPolicyOptions::default();
    let mut json = false;
    let mut manifest_dir: Option<PathBuf> = None;
    let mut now: Option<i64> = None;
    let mut filter: Option<String> = None;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--json" => json = true,
            "--filter" => {
                if filter.is_some() {
                    bail!("--filter may be specified only once");
                }
                index += 1;
                filter = Some(
                    arguments
                        .get(index)
                        .context("--filter requires a test name")?
                        .clone(),
                );
            }
            "--now" => {
                if now.is_some() {
                    bail!("--now may be specified only once");
                }
                index += 1;
                let value = arguments
                    .get(index)
                    .context("--now requires a Unix epoch second")?;
                now = Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| anyhow!("--now must be a Unix epoch second"))?,
                );
            }
            "--manifest-dir" => {
                index += 1;
                manifest_dir = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--manifest-dir requires a directory")?,
                ));
            }
            _ if policy.parse_option(arguments, &mut index)? => {}
            option if option.starts_with("--") => bail!("unknown test option {option:?}"),
            path => roots.push(PathBuf::from(path)),
        }
        index += 1;
    }
    policy.load()?;
    let manifest_dir = manifest_dir.map(std::fs::canonicalize).transpose()?;
    if roots.is_empty() {
        let tests = PathBuf::from("tests");
        roots.push(if tests.is_dir() {
            tests
        } else {
            PathBuf::from(".")
        });
    }
    let files = discover(&roots)?;
    if files.is_empty() {
        bail!("no {PROGRAM_SUFFIX} files found under the given paths");
    }
    let (files, not_run) = select_by_name(files, filter.as_deref())?;
    let executable = std::env::current_exe().context("locating the execute executable")?;
    let mut outcomes = Vec::new();
    for file in &files {
        let outcome = match run_one(&executable, file, manifest_dir.as_deref(), &policy, now) {
            Ok(outcome) => outcome,
            Err(error) => Outcome {
                name: test_name(file),
                file: file.clone(),
                failures: vec![format!("could not run test: {error:#}")],
                exit: None,
                stdout: String::new(),
                stderr: String::new(),
            },
        };
        outcomes.push(outcome);
    }
    let failed = outcomes.iter().filter(|outcome| !outcome.passed()).count();
    let output = if json {
        serde_json::to_string_pretty(&json_report(&outcomes, &not_run))?
    } else {
        tap_report(&outcomes, &not_run)
    };
    writeln!(io::stdout(), "{output}")?;
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Splits `files` into the tests that `filter` names and the names of the rest, which
/// are reported as not run. With no filter every test is selected. A filter that names
/// no test is an error that lists the tests that were found.
fn select_by_name(
    files: Vec<PathBuf>,
    filter: Option<&str>,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let Some(wanted) = filter else {
        return Ok((files, Vec::new()));
    };
    let (selected, rest): (Vec<PathBuf>, Vec<PathBuf>) = files
        .into_iter()
        .partition(|file| test_name(file) == wanted);
    if selected.is_empty() {
        let found: Vec<String> = rest.iter().map(|file| test_name(file)).collect();
        bail!(
            "--filter {wanted:?} matches no test; tests found: {}",
            found.join(", ")
        );
    }
    let not_run = rest.iter().map(|file| test_name(file)).collect();
    Ok((selected, not_run))
}

/// Collect `*.test.program` files under each root, in sorted order.
pub fn discover(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for root in roots {
        if root.is_file() {
            if !is_test_program(root) {
                bail!("{} is not a {PROGRAM_SUFFIX} file", root.display());
            }
            found.push(root.clone());
        } else if root.is_dir() {
            collect(root, 0, &mut found)?;
        } else {
            bail!("{} does not exist", root.display());
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn collect(directory: &Path, depth: usize, found: &mut Vec<PathBuf>) -> Result<()> {
    if depth > MAX_DISCOVERY_DEPTH {
        bail!("test discovery exceeds {MAX_DISCOVERY_DEPTH} directory levels");
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(directory)
        .with_context(|| format!("reading {}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    for path in entries {
        let hidden = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        if path.is_dir() {
            if !hidden {
                collect(&path, depth + 1, found)?;
            }
        } else if is_test_program(&path) {
            found.push(path);
        }
    }
    Ok(())
}

fn is_test_program(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(PROGRAM_SUFFIX))
}

/// `login.test.program` is reported as `login`.
pub fn test_name(file: &Path) -> String {
    file.file_name()
        .map(|name| {
            name.to_string_lossy()
                .trim_end_matches(PROGRAM_SUFFIX)
                .to_owned()
        })
        .unwrap_or_default()
}

/// The sidecar sits beside the program: `NAME.test.program` uses `NAME.test.json`.
pub fn sidecar_path(file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map(|name| {
            name.to_string_lossy()
                .trim_end_matches(".program")
                .to_owned()
        })
        .unwrap_or_default();
    file.with_file_name(format!("{name}.json"))
}

pub fn load_sidecar(file: &Path) -> Result<Sidecar> {
    let path = sidecar_path(file);
    if !path.exists() {
        return Ok(Sidecar::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn run_one(
    executable: &Path,
    file: &Path,
    manifest_dir: Option<&Path>,
    policy: &crate::execution_policy_options::ExecutionPolicyOptions,
    now: Option<i64>,
) -> Result<Outcome> {
    let sidecar = load_sidecar(file)?;
    let program = fs::canonicalize(file)?;
    // The guard removes the directory when this function returns, on success or error.
    let work = WorkDir::create()?;
    let (exit, stdout, stderr, failures) = run_in(
        executable,
        &program,
        &sidecar,
        manifest_dir,
        policy,
        now,
        work.path(),
    )?;
    Ok(Outcome {
        name: test_name(file),
        file: file.to_path_buf(),
        failures,
        exit,
        stdout,
        stderr,
    })
}

type RunResult = (Option<i32>, String, String, Vec<String>);

fn run_in(
    executable: &Path,
    program: &Path,
    sidecar: &Sidecar,
    manifest_dir: Option<&Path>,
    policy: &crate::execution_policy_options::ExecutionPolicyOptions,
    now: Option<i64>,
    work: &Path,
) -> Result<RunResult> {
    let fixtures = program
        .parent()
        .map(|parent| parent.join("fixtures"))
        .unwrap_or_else(|| PathBuf::from("fixtures"));
    let mut command = Command::new(executable);
    command.current_dir(work).arg(program);
    // A per-test `now` wins over the run-wide `--now`; either pins the
    // injectable clock the program reads through `time.now`.
    if let Some(pinned) = sidecar.now.or(now) {
        command.env("EXECUTE_NOW", pinned.to_string());
    }
    if let Some(manifest_dir) = manifest_dir {
        command.arg("--manifest-dir").arg(manifest_dir);
    }
    policy.forward(&mut command);
    for (name, value) in &sidecar.inputs {
        command.arg("--input").arg(format!("{name}={value}"));
    }
    if !sidecar.arguments.is_empty() {
        command.arg("--").args(&sidecar.arguments);
    }
    let bin = fixtures.join("bin");
    let mut path = std::ffi::OsString::new();
    if bin.is_dir() {
        path.push(&bin);
        path.push(if cfg!(windows) { ";" } else { ":" });
    }
    path.push(std::env::var_os("PATH").unwrap_or_default());
    command.env("PATH", path);
    command.env("EXECUTE_TEST_FIXTURES", fixtures);
    let output = command
        .output()
        .with_context(|| format!("starting {}", executable.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit = output.status.code();

    let mut failures = Vec::new();
    let expected_status = sidecar.expect.status;
    if exit != Some(expected_status) {
        failures.push(format!(
            "exit status {} but expected {expected_status}",
            exit.map_or("none (killed by a signal)".to_owned(), |code| code
                .to_string())
        ));
    }
    failures.extend(stream_failures(
        "stdout",
        &stdout,
        sidecar.expect.stdout.as_deref(),
        &[],
    ));
    failures.extend(stream_failures(
        "stderr",
        &stderr,
        sidecar.expect.stderr.as_deref(),
        &sidecar.expect.stderr_contains,
    ));
    for (relative, expected) in &sidecar.expect.files {
        let path = work.join(relative);
        match fs::read_to_string(&path) {
            Ok(actual) if &actual == expected => {}
            Ok(actual) => failures.push(format!(
                "{relative} is {actual:?} but expected {expected:?}"
            )),
            Err(_) => failures.push(format!("{relative} was not created")),
        }
    }
    Ok((exit, stdout, stderr, failures))
}

/// Checks one captured stream against its expectations and returns one message per failed
/// check. `exact`, when set, must equal `actual` byte for byte; each entry of `contains`
/// must occur somewhere in `actual`. Pure, so it is tested without starting a program.
fn stream_failures(
    label: &str,
    actual: &str,
    exact: Option<&str>,
    contains: &[String],
) -> Vec<String> {
    let mut failures = Vec::new();
    if let Some(expected) = exact {
        if actual != expected {
            failures.push(format!("{label} {actual:?} but expected {expected:?}"));
        }
    }
    for needle in contains {
        if !actual.contains(needle.as_str()) {
            failures.push(format!("{label} {actual:?} does not contain {needle:?}"));
        }
    }
    failures
}

/// A private working directory for one test. Its name carries 16 random bytes, and it is
/// created with `DirBuilder` rather than `create_dir_all`, so an existing path (or a
/// planted symbolic link) is an error rather than a directory the test would reuse. On Unix
/// the directory is mode 0700. The guard removes the tree when dropped.
pub struct WorkDir {
    path: PathBuf,
}

const WORK_DIR_ATTEMPTS: usize = 8;

impl WorkDir {
    pub fn create() -> Result<Self> {
        Self::create_in(&std::env::temp_dir())
    }

    fn create_in(parent: &Path) -> Result<Self> {
        for _ in 0..WORK_DIR_ATTEMPTS {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random)
                .map_err(|error| anyhow!("random directory name: {error}"))?;
            let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let path = parent.join(format!("execute-test-{name}"));
            // `mut` is needed only where the Unix mode is set below.
            #[allow(unused_mut)]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()))
                }
            }
        }
        bail!("could not create a unique test directory after {WORK_DIR_ATTEMPTS} attempts")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        // A failure here leaves files behind but cannot change the test result.
        let _ = remove_tree(&self.path);
    }
}

/// Remove a directory tree without following symbolic links: a link is removed as a
/// link, and its target is left alone.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// TAP 13. Strings in diagnostics are JSON-quoted, which is valid YAML. Tests named in
/// `not_run` follow the run tests as skipped.
pub fn tap_report(outcomes: &[Outcome], not_run: &[String]) -> String {
    let mut lines = vec![
        "TAP version 13".to_owned(),
        format!("1..{}", outcomes.len() + not_run.len()),
    ];
    for (index, outcome) in outcomes.iter().enumerate() {
        let number = index + 1;
        if outcome.passed() {
            lines.push(format!("ok {number} - {}", outcome.name));
        } else {
            lines.push(format!("not ok {number} - {}", outcome.name));
            lines.push("  ---".to_owned());
            lines.push(format!(
                "  file: {}",
                json!(outcome.file.display().to_string())
            ));
            lines.push("  failures:".to_owned());
            for failure in &outcome.failures {
                lines.push(format!("    - {}", json!(failure)));
            }
            if !outcome.stderr.is_empty() {
                lines.push(format!("  stderr: {}", json!(outcome.stderr)));
            }
            lines.push("  ...".to_owned());
        }
    }
    for (offset, name) in not_run.iter().enumerate() {
        let number = outcomes.len() + offset + 1;
        lines.push(format!(
            "ok {number} - {name} # SKIP not selected by --filter"
        ));
    }
    lines.join("\n")
}

/// The JSON document: one entry per test, the names `--filter` left out, and a summary.
pub fn json_report(outcomes: &[Outcome], not_run: &[String]) -> Value {
    let passed = outcomes.iter().filter(|outcome| outcome.passed()).count();
    json!({
        "protocol": "execute.test/1",
        "summary": {
            "total": outcomes.len(),
            "passed": passed,
            "failed": outcomes.len() - passed,
        },
        "tests": outcomes.iter().map(|outcome| json!({
            "name": outcome.name,
            "file": outcome.file.display().to_string(),
            "status": if outcome.passed() { "pass" } else { "fail" },
            "exit": outcome.exit,
            "stdout": outcome.stdout,
            "failures": outcome.failures,
        })).collect::<Vec<_>>(),
        "not_run": not_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_directory_is_created_private_and_removed() {
        let parent = tempfile::tempdir().unwrap();
        let path;
        {
            let work = WorkDir::create_in(parent.path()).unwrap();
            path = work.path().to_path_buf();
            assert!(path.is_dir());
            assert!(path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("execute-test-"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            fs::write(path.join("file.txt"), "x").unwrap();
            fs::create_dir(path.join("nested")).unwrap();
            fs::write(path.join("nested/inner.txt"), "y").unwrap();
        }
        assert!(!path.exists(), "the guard removes the tree on drop");
    }

    #[cfg(unix)]
    #[test]
    fn removing_a_work_directory_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep.txt"), "keep").unwrap();
        let path;
        {
            let work = WorkDir::create_in(parent.path()).unwrap();
            path = work.path().to_path_buf();
            symlink(outside.path(), path.join("link")).unwrap();
        }
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(outside.path().join("keep.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn names_and_sidecars_follow_the_test_file() {
        let file = Path::new("tests/login.test.program");
        assert_eq!(test_name(file), "login");
        assert_eq!(sidecar_path(file), PathBuf::from("tests/login.test.json"));
    }

    #[test]
    fn sidecar_rejects_unknown_fields_and_defaults_to_success() {
        let parsed: Sidecar = serde_json::from_str(r#"{"inputs": {"a": "b"}}"#).unwrap();
        assert_eq!(parsed.expect.status, 0);
        assert_eq!(parsed.inputs["a"], "b");
        assert!(serde_json::from_str::<Sidecar>(r#"{"expected": 1}"#).is_err());
        assert!(serde_json::from_str::<Sidecar>(r#"{"expect": {"stdot": "x"}}"#).is_err());
    }

    #[test]
    fn sidecar_accepts_stderr_expectations_and_rejects_near_misses() {
        let parsed: Sidecar = serde_json::from_str(
            r#"{"expect": {"stderr": "warn\n", "stderr_contains": ["disk", "full"]}}"#,
        )
        .unwrap();
        assert_eq!(parsed.expect.stderr.as_deref(), Some("warn\n"));
        assert_eq!(parsed.expect.stderr_contains, vec!["disk", "full"]);
        let misspelt = r#"{"expect": {"stderr_contain": ["x"]}}"#;
        assert!(serde_json::from_str::<Sidecar>(misspelt).is_err());
        let wrong_type = r#"{"expect": {"stderr": ["x"]}}"#;
        assert!(serde_json::from_str::<Sidecar>(wrong_type).is_err());
    }

    #[test]
    fn stderr_exact_match_is_byte_for_byte() {
        assert!(stream_failures("stderr", "warn\n", Some("warn\n"), &[]).is_empty());
        assert_eq!(
            stream_failures("stderr", "warn\n", Some("warn"), &[]),
            vec![r#"stderr "warn\n" but expected "warn""#.to_string()]
        );
        assert_eq!(
            stream_failures("stderr", "", Some("warn\n"), &[]),
            vec![r#"stderr "" but expected "warn\n""#.to_string()]
        );
    }

    #[test]
    fn stderr_contains_checks_each_substring() {
        let needles = vec!["disk".to_string(), "full".to_string()];
        assert!(stream_failures("stderr", "disk is full", None, &needles).is_empty());
        assert_eq!(
            stream_failures("stderr", "disk is low", None, &needles),
            vec![r#"stderr "disk is low" does not contain "full""#.to_string()]
        );
        // Both substrings missing gives two messages, one per failed check.
        assert_eq!(stream_failures("stderr", "", None, &needles).len(), 2);
    }

    #[test]
    fn stream_checks_are_skipped_when_nothing_is_expected() {
        assert!(stream_failures("stdout", "anything", None, &[]).is_empty());
    }

    #[test]
    fn tap_marks_failures_and_keeps_order() {
        let outcomes = vec![
            Outcome {
                name: "a".into(),
                file: PathBuf::from("a.test.program"),
                failures: vec![],
                exit: Some(0),
                stdout: "ok".into(),
                stderr: String::new(),
            },
            Outcome {
                name: "b".into(),
                file: PathBuf::from("b.test.program"),
                failures: vec!["exit status 1 but expected 0".into()],
                exit: Some(1),
                stdout: String::new(),
                stderr: "boom".into(),
            },
        ];
        let tap = tap_report(&outcomes, &[]);
        assert!(
            tap.starts_with("TAP version 13\n1..2\nok 1 - a\nnot ok 2 - b\n"),
            "{tap}"
        );
        assert!(tap.contains("stderr: \"boom\""), "{tap}");
        let json = json_report(&outcomes, &[]);
        assert_eq!(json["summary"]["failed"], 1);
        assert_eq!(json["tests"][1]["status"], "fail");
    }

    #[test]
    fn filter_selects_one_test_and_names_the_rest_as_not_run() {
        let files = vec![
            PathBuf::from("tests/a.test.program"),
            PathBuf::from("tests/b.test.program"),
            PathBuf::from("tests/c.test.program"),
        ];
        let (selected, not_run) = select_by_name(files.clone(), Some("b")).unwrap();
        assert_eq!(selected, vec![PathBuf::from("tests/b.test.program")]);
        assert_eq!(not_run, vec!["a", "c"]);
        let (all, none) = select_by_name(files.clone(), None).unwrap();
        assert_eq!(all, files);
        assert!(none.is_empty());
    }

    #[test]
    fn filter_naming_no_test_lists_the_tests_found() {
        let files = vec![
            PathBuf::from("a.test.program"),
            PathBuf::from("b.test.program"),
        ];
        let error = select_by_name(files, Some("zz")).unwrap_err().to_string();
        assert!(error.contains("matches no test"), "{error}");
        assert!(error.contains("a, b"), "{error}");
    }

    #[test]
    fn tap_numbers_not_run_tests_as_skipped_after_the_run_ones() {
        let outcomes = vec![Outcome {
            name: "b".into(),
            file: PathBuf::from("b.test.program"),
            failures: vec![],
            exit: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }];
        let not_run = vec!["a".to_string(), "c".to_string()];
        let tap = tap_report(&outcomes, &not_run);
        assert!(tap.starts_with("TAP version 13\n1..3\nok 1 - b\n"), "{tap}");
        assert!(
            tap.contains("ok 2 - a # SKIP not selected by --filter"),
            "{tap}"
        );
        assert!(
            tap.contains("ok 3 - c # SKIP not selected by --filter"),
            "{tap}"
        );
        assert_eq!(json_report(&outcomes, &not_run)["not_run"][1], "c");
    }

    #[test]
    fn discovery_finds_only_test_programs_in_sorted_order() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("b.test.program"), "").unwrap();
        fs::write(directory.path().join("a.test.program"), "").unwrap();
        fs::write(directory.path().join("plain.program"), "").unwrap();
        fs::write(directory.path().join("b.test.json"), "{}").unwrap();
        let found = discover(&[directory.path().to_path_buf()]).unwrap();
        let names: Vec<String> = found.iter().map(|path| test_name(path)).collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
