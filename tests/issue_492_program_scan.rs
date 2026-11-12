//! Issue #492: scanning a directory lists the interface of each `.program` file that parses,
//! and reports the files that do not parse without aborting the scan.

use flow::{
    manifest::Registry,
    program_files::{scan_program_directory, Access, ProgramScanEntry},
};
use std::fs;
use std::path::Path;

const THEME_PROGRAM: &str = "accept theme as text\nrun cat with path theme\n";
const COUNT_PROGRAM: &str = "accept count as integer with default 3\nrun cat with path count\n";
const BROKEN_PROGRAM: &str = "accept theme as no-such-type\n";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn scan(dir: &Path, recursive: bool) -> Vec<ProgramScanEntry> {
    let registry = Registry::default();
    scan_program_directory(
        dir,
        recursive,
        Access {
            registry: &registry,
            cwd: dir,
        },
    )
    .unwrap()
}

fn file_name(entry: &ProgramScanEntry) -> String {
    entry
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn lists_parsing_programs_with_inputs_and_reports_broken_ones() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("theme.program"), THEME_PROGRAM);
    write(&dir.path().join("count.program"), COUNT_PROGRAM);
    write(&dir.path().join("broken.program"), BROKEN_PROGRAM);
    write(&dir.path().join("notes.txt"), "not a program");
    write(&dir.path().join("nested/inner.program"), THEME_PROGRAM);

    let entries = scan(dir.path(), false);
    let names: Vec<String> = entries.iter().map(file_name).collect();
    assert_eq!(names, ["broken.program", "count.program", "theme.program"]);

    match &entries[0] {
        ProgramScanEntry::Failed { error, .. } => assert!(!error.is_empty()),
        other => panic!("expected a failure for broken.program, got {other:?}"),
    }

    let ProgramScanEntry::Interface { interface, .. } = &entries[2] else {
        panic!("expected an interface for theme.program");
    };
    let inputs = interface["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["name"], "theme");
    assert_eq!(inputs[0]["type"], "text");
    assert_eq!(inputs[0]["required"], true);

    let ProgramScanEntry::Interface { interface, .. } = &entries[1] else {
        panic!("expected an interface for count.program");
    };
    let inputs = interface["inputs"].as_array().unwrap();
    assert_eq!(inputs[0]["name"], "count");
    assert_eq!(inputs[0]["type"], "integer");
    assert_eq!(inputs[0]["required"], false);
    assert_eq!(inputs[0]["default"], 3);
}

#[test]
fn recursive_scan_enters_subdirectories() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("theme.program"), THEME_PROGRAM);
    write(&dir.path().join("nested/inner.program"), THEME_PROGRAM);

    assert_eq!(scan(dir.path(), false).len(), 1);
    let names: Vec<String> = scan(dir.path(), true).iter().map(file_name).collect();
    assert_eq!(names, ["inner.program", "theme.program"]);
}

#[test]
fn missing_directory_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::default();
    let missing = dir.path().join("absent");
    let result = scan_program_directory(
        &missing,
        false,
        Access {
            registry: &registry,
            cwd: dir.path(),
        },
    );
    assert!(result.is_err());
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_never_followed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("far.program"), THEME_PROGRAM);
    write(&dir.path().join("real.program"), THEME_PROGRAM);
    std::os::unix::fs::symlink(
        outside.path().join("far.program"),
        dir.path().join("linked.program"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked-dir")).unwrap();

    let entries = scan(dir.path(), true);
    let names: Vec<String> = entries.iter().map(file_name).collect();
    assert_eq!(names, ["real.program"]);
}
