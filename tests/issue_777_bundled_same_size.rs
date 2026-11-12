//! Issue #777: the applet-contract guide says a same-size edit to a bundled
//! manifest is detected, and quotes the mismatch diagnostic. These tests hold
//! the documented wording to the current code.

use std::fs;
use std::path::Path;

use flow::manifest::{ManifestCheck, Registry};

fn copy_shipped_manifests(dir: &Path) {
    for entry in fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap() {
        let entry = entry.unwrap();
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        }
    }
}

#[test]
fn same_size_edit_to_bundled_manifest_fails_closed_with_documented_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    copy_shipped_manifests(dir.path());
    let path = dir.path().join("awk.json");
    let original = fs::read(&path).unwrap();
    let position = original
        .iter()
        .position(|byte| *byte == b' ')
        .expect("awk.json contains a space");
    let mut edited = original.clone();
    edited[position] = b'\t';
    fs::write(&path, &edited).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        original.len() as u64,
        "the edit must keep the byte size"
    );

    // Lazy: the default directory loads, and the edited applet fails on first use.
    let lazy = Registry::load_default(dir.path(), ManifestCheck::Lazy).unwrap();
    assert_eq!(lazy.resolve("cat").unwrap().name, "cat");
    let lazy_error = format!("{:#}", lazy.resolve("awk").expect_err("edited awk refused"));
    assert!(
        lazy_error.starts_with("manifest awk.json in "),
        "{lazy_error}"
    );
    assert!(
        lazy_error.contains(" differs from the bundled copy; pass --manifest-dir "),
        "{lazy_error}"
    );
    assert!(
        lazy_error.ends_with(" to load edited manifests as files"),
        "{lazy_error}"
    );

    // Full (the `check` command): the same diagnostic at load time.
    let full_error = format!(
        "{:#}",
        Registry::load_default(dir.path(), ManifestCheck::Full)
            .expect_err("full check refuses the edited directory")
    );
    assert_eq!(full_error, lazy_error);
}

#[test]
fn explicit_manifest_dir_loads_an_intentionally_edited_manifest() {
    let dir = tempfile::tempdir().unwrap();
    copy_shipped_manifests(dir.path());
    let path = dir.path().join("awk.json");
    let mut edited = fs::read_to_string(&path).unwrap();
    edited.push(' ');
    fs::write(&path, edited).unwrap();

    // An explicit directory is parsed and fully validated, not compared with the index.
    let registry = Registry::load(dir.path()).unwrap();
    assert_eq!(registry.resolve("awk").unwrap().name, "awk");
}
