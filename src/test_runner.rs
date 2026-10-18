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
//! tests. `EXECUTE_TEST_FIXTURES` names the `fixtures` directory. `stdout` is an exact
//! byte comparison; `files` maps paths relative to the working directory to their exact
//! contents. Output is TAP 13 by default, and `--json` prints one document for agents.
//! The exit status is 0 only when every test passes.

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
    pub expect: Expect,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    #[serde(default)]
    pub status: i32,
    #[serde(default)]
    pub stdout: Option<String>,
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
    let mut json = false;
    let mut manifest_dir: Option<PathBuf> = None;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--json" => json = true,
            "--manifest-dir" => {
                index += 1;
                manifest_dir = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--manifest-dir requires a directory")?,
                ));
            }
            option if option.starts_with("--") => bail!("unknown test option {option:?}"),
            path => roots.push(PathBuf::from(path)),
        }
        index += 1;
    }
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
    let executable = std::env::current_exe().context("locating the execute executable")?;
    let mut outcomes = Vec::new();
    for file in &files {
        let outcome = match run_one(&executable, file, manifest_dir.as_deref()) {
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
        serde_json::to_string_pretty(&json_report(&outcomes))?
    } else {
        tap_report(&outcomes)
    };
    writeln!(io::stdout(), "{output}")?;
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
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

fn run_one(executable: &Path, file: &Path, manifest_dir: Option<&Path>) -> Result<Outcome> {
    let sidecar = load_sidecar(file)?;
    let program = fs::canonicalize(file)?;
    let fixtures = program
        .parent()
        .map(|parent| parent.join("fixtures"))
        .unwrap_or_else(|| PathBuf::from("fixtures"));
    // The guard removes the directory when this function returns, on success or error.
    let work = WorkDir::create()?;
    let (exit, stdout, stderr, failures) = run_in(
        executable,
        &program,
        &sidecar,
        &fixtures,
        manifest_dir,
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
    fixtures: &Path,
    manifest_dir: Option<&Path>,
    work: &Path,
) -> Result<RunResult> {
    let mut command = Command::new(executable);
    command.current_dir(work).arg(program);
    if let Some(manifest_dir) = manifest_dir {
        command.arg("--manifest-dir").arg(manifest_dir);
    }
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
    if let Some(expected) = &sidecar.expect.stdout {
        if &stdout != expected {
            failures.push(format!("stdout {stdout:?} but expected {expected:?}"));
        }
    }
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

/// TAP 13. Strings in diagnostics are JSON-quoted, which is valid YAML.
pub fn tap_report(outcomes: &[Outcome]) -> String {
    let mut lines = vec![
        "TAP version 13".to_owned(),
        format!("1..{}", outcomes.len()),
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
    lines.join("\n")
}

/// The JSON document: one entry per test and a summary.
pub fn json_report(outcomes: &[Outcome]) -> Value {
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
        let tap = tap_report(&outcomes);
        assert!(
            tap.starts_with("TAP version 13\n1..2\nok 1 - a\nnot ok 2 - b\n"),
            "{tap}"
        );
        assert!(tap.contains("stderr: \"boom\""), "{tap}");
        let json = json_report(&outcomes);
        assert_eq!(json["summary"]["failed"], 1);
        assert_eq!(json["tests"][1]["status"], "fail");
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
