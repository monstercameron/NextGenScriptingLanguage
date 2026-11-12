use flow::modules::{
    generate_program_module_manifest, install_applet_package, parse_applet_index,
    remove_applet_package, review_applet_package, search_applet_index, verify_applet_package_dir,
    AppletIndexFile, AppletIndexPackage, AppletInstallLock, AppletInstallOutcome, AppletReview,
    APPLET_PACKAGE_MANIFEST_FILE,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const SOURCE: &str = "define a program named describe\n  accept text as text\n  run \"printf\" with arguments [\"{\\\"text\\\":\\\"%s\\\"}\", text]\n";
const LIBRARY: &str = "src/library.program";
const SOURCE_URL: &str = "https://example.invalid/packages/notes-1.0.0.tar";

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn zero_digest() -> String {
    "0".repeat(64)
}

/// Writes a two-file package (manifest and program source) and returns the
/// index entry that describes its current bytes.
fn write_package(
    dir: &Path,
    name: &str,
    capabilities: &[&str],
    effects: &[&str],
) -> AppletIndexPackage {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join(LIBRARY), SOURCE).unwrap();
    let text = generate_program_module_manifest(&dir.join(LIBRARY), dir, name, "1.0.0", "describe")
        .unwrap();
    let mut manifest: Value = serde_json::from_str(&text).unwrap();
    manifest["exports"]["describe"]["capabilities"] = json!(capabilities);
    manifest["exports"]["describe"]["effects"] = json!(effects);
    fs::write(
        dir.join(APPLET_PACKAGE_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    package_entry(dir, name)
}

fn package_entry(dir: &Path, name: &str) -> AppletIndexPackage {
    let mut files = Vec::new();
    for path in [APPLET_PACKAGE_MANIFEST_FILE, LIBRARY] {
        let bytes = fs::read(dir.join(path)).unwrap();
        files.push(AppletIndexFile {
            path: path.to_owned(),
            sha256: digest(&bytes),
            size: u64::try_from(bytes.len()).unwrap(),
        });
    }
    AppletIndexPackage {
        name: name.to_owned(),
        version: "1.0.0".into(),
        description: Some("Notes helpers".into()),
        source: SOURCE_URL.into(),
        files,
    }
}

fn index_entry(name: &str, description: Option<&str>) -> Value {
    let mut entry = json!({
        "name": name,
        "version": "1.0.0",
        "source": SOURCE_URL,
        "files": [{"path": APPLET_PACKAGE_MANIFEST_FILE, "sha256": zero_digest(), "size": 1}],
    });
    if let Some(description) = description {
        entry["description"] = json!(description);
    }
    entry
}

fn index_json(packages: Value) -> String {
    json!({"format_version": 1, "packages": packages}).to_string()
}

fn refusal(text: &str) -> String {
    format!("{:#}", parse_applet_index(text.as_bytes()).unwrap_err())
}

#[test]
fn index_parses_strictly_and_search_is_deterministic() {
    let text = index_json(json!([
        index_entry("zeta.tools", Some("Calendar helpers")),
        index_entry("alpha", None),
    ]));
    let index = parse_applet_index(text.as_bytes()).unwrap();
    let names = |query: &str| {
        search_applet_index(&index, query)
            .iter()
            .map(|package| package.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(""), ["alpha", "zeta.tools"]);
    assert_eq!(names("ALP"), ["alpha"]);
    assert_eq!(names("calendar"), ["zeta.tools"]);
    assert!(names("missing").is_empty());

    let unknown_top = json!({"format_version": 1, "packages": [], "extra": true}).to_string();
    assert!(refusal(&unknown_top).contains("unknown field"));

    let mut unknown_package = index_entry("alpha", None);
    unknown_package["maintainer"] = json!("someone");
    assert!(refusal(&index_json(json!([unknown_package]))).contains("unknown field"));

    let wrong_format = json!({"format_version": 2, "packages": []}).to_string();
    assert!(refusal(&wrong_format).contains("unsupported applet index format"));

    let duplicate = index_json(json!([
        index_entry("alpha", None),
        index_entry("ALPHA", None)
    ]));
    assert!(refusal(&duplicate).contains("more than once"));

    assert!(refusal(&index_json(json!([index_entry("../evil", None)])))
        .contains("invalid applet package name"));

    let mut bad_version = index_entry("alpha", None);
    bad_version["version"] = json!("1.0");
    assert!(refusal(&index_json(json!([bad_version]))).contains("version"));

    let mut no_source = index_entry("alpha", None);
    no_source["source"] = json!("");
    assert!(refusal(&index_json(json!([no_source]))).contains("source must be"));

    let mut bad_digest = index_entry("alpha", None);
    bad_digest["files"][0]["sha256"] = json!("XYZ");
    assert!(refusal(&index_json(json!([bad_digest]))).contains("64-character"));

    let mut no_files = index_entry("alpha", None);
    no_files["files"] = json!([]);
    assert!(refusal(&index_json(json!([no_files]))).contains("between 1 and"));
}

#[test]
fn hostile_listed_paths_are_refused_before_the_filesystem() {
    let hostile = [
        "../escape.program",
        "/etc/passwd",
        "src\\main.program",
        "C:/evil.program",
        "C:evil.program",
        "src/../../x",
        "./a.program",
        "src//a.program",
        "src/./a.program",
        "src/",
        "",
        "src/\u{7}.program",
    ];
    // The directory does not exist: any filesystem access would fail with a
    // different message, so "unsafe path" proves the path check ran first.
    let missing = Path::new("definitely/not/a/real/package/dir");
    for path in hostile {
        let mut entry = index_entry("alpha", None);
        entry["files"][0]["path"] = json!(path);
        assert!(
            parse_applet_index(index_json(json!([entry])).as_bytes()).is_err(),
            "{path:?} parsed"
        );
        let package = AppletIndexPackage {
            name: "alpha".into(),
            version: "1.0.0".into(),
            description: None,
            source: SOURCE_URL.into(),
            files: vec![AppletIndexFile {
                path: path.into(),
                sha256: zero_digest(),
                size: 1,
            }],
        };
        let error = format!(
            "{:#}",
            verify_applet_package_dir(&package, missing).unwrap_err()
        );
        assert!(error.contains("unsafe path"), "{path:?}: {error}");
    }
}

#[test]
fn verify_names_the_file_for_extra_missing_and_mismatched_content() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join("downloaded");
    let package = write_package(&dir, "notes", &[], &[]);
    verify_applet_package_dir(&package, &dir).unwrap();
    let original = fs::read(dir.join(LIBRARY)).unwrap();

    fs::write(dir.join("src/extra.program"), "unlisted").unwrap();
    let error = format!(
        "{:#}",
        verify_applet_package_dir(&package, &dir).unwrap_err()
    );
    assert!(
        error.contains("unlisted file \"src/extra.program\""),
        "{error}"
    );
    fs::remove_file(dir.join("src/extra.program")).unwrap();

    fs::create_dir(dir.join("stray")).unwrap();
    let error = format!(
        "{:#}",
        verify_applet_package_dir(&package, &dir).unwrap_err()
    );
    assert!(error.contains("unlisted directory \"stray\""), "{error}");
    fs::remove_dir(dir.join("stray")).unwrap();

    let edited = SOURCE.replacen("describe", "Describe", 1);
    assert_eq!(edited.len(), SOURCE.len());
    fs::write(dir.join(LIBRARY), &edited).unwrap();
    let listed = package
        .files
        .iter()
        .find(|file| file.path == LIBRARY)
        .unwrap()
        .sha256
        .clone();
    let error = format!(
        "{:#}",
        verify_applet_package_dir(&package, &dir).unwrap_err()
    );
    assert!(
        error.contains("\"src/library.program\" SHA-256 mismatch"),
        "{error}"
    );
    assert!(error.contains(&listed), "{error}");
    assert!(error.contains(&digest(edited.as_bytes())), "{error}");

    fs::write(dir.join(LIBRARY), format!("{SOURCE}# more\n")).unwrap();
    let error = format!(
        "{:#}",
        verify_applet_package_dir(&package, &dir).unwrap_err()
    );
    assert!(error.contains("\"src/library.program\" is"), "{error}");
    assert!(error.contains("bytes, but the index lists"), "{error}");

    fs::remove_file(dir.join(LIBRARY)).unwrap();
    let error = format!(
        "{:#}",
        verify_applet_package_dir(&package, &dir).unwrap_err()
    );
    assert!(
        error.contains("missing listed file \"src/library.program\""),
        "{error}"
    );

    fs::write(dir.join(LIBRARY), &original).unwrap();
    verify_applet_package_dir(&package, &dir).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinks_inside_a_package_are_refused() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join("downloaded");
    let package = write_package(&dir, "notes", &[], &[]);
    std::os::unix::fs::symlink(dir.join(LIBRARY), dir.join("src/alias.program")).unwrap();
    let error = format!(
        "{:#}",
        verify_applet_package_dir(&package, &dir).unwrap_err()
    );
    assert!(error.contains("symbolic link"), "{error}");
}

#[test]
fn review_reports_declared_capabilities_and_refuses_undeclared_effects() {
    let temp = tempdir().unwrap();
    let good = temp.path().join("good");
    let package = write_package(&good, "notes", &["filesystem.read"], &["filesystem.read"]);
    let review = review_applet_package(&package, &good).unwrap();
    assert!(review.refusals.is_empty(), "{:?}", review.refusals);
    assert_eq!(review.exports.len(), 1);
    assert_eq!(review.exports[0].name, "describe");
    assert_eq!(review.exports[0].capabilities, ["filesystem.read"]);
    assert_eq!(review.exports[0].effects[0].effect, "filesystem.read");
    assert_eq!(review.exports[0].effects[0].category, "reads");

    let bad = temp.path().join("bad");
    let package = write_package(&bad, "undeclared", &[], &["process.exec"]);
    let review = review_applet_package(&package, &bad).unwrap();
    assert!(review.exports.is_empty());
    assert_eq!(review.refusals.len(), 1);
    assert!(
        review.refusals[0].contains("effects must be unique and included in its capabilities"),
        "{:?}",
        review.refusals
    );
}

#[test]
fn install_writes_a_lock_and_a_second_install_is_unchanged() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("downloaded");
    let user = temp.path().join("user");
    fs::create_dir(&user).unwrap();
    let package = write_package(&source, "notes", &["filesystem.read"], &["filesystem.read"]);
    let shown = review_applet_package(&package, &source).unwrap();

    let first = install_applet_package(&package, &source, &user, &shown).unwrap();
    assert_eq!(first.outcome, AppletInstallOutcome::Installed);
    assert_eq!(
        first.directory,
        fs::canonicalize(&user).unwrap().join("notes")
    );
    assert_eq!(
        fs::read(first.directory.join(LIBRARY)).unwrap(),
        SOURCE.as_bytes()
    );

    let lock: AppletInstallLock =
        serde_json::from_slice(&fs::read(&first.lock_path).unwrap()).unwrap();
    assert_eq!(lock.root, "notes");
    assert_eq!(lock.source, SOURCE_URL);
    assert_eq!(lock.packages["notes"].version, "1.0.0");
    assert_eq!(
        lock.packages["notes"].manifest,
        PathBuf::from(APPLET_PACKAGE_MANIFEST_FILE)
    );
    assert_eq!(
        lock.packages["notes"].source_sha256,
        digest(SOURCE.as_bytes())
    );
    assert_eq!(lock.files.len(), 2);
    assert_eq!(lock.files[LIBRARY].sha256, digest(SOURCE.as_bytes()));
    assert_eq!(
        lock.files[LIBRARY].size,
        u64::try_from(SOURCE.len()).unwrap()
    );

    let second = install_applet_package(&package, &source, &user, &shown).unwrap();
    assert_eq!(second.outcome, AppletInstallOutcome::Unchanged);
    let leftovers = fs::read_dir(&user)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.'))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    verify_applet_package_dir(&package, &first.directory).unwrap();
}

#[test]
fn install_refuses_unshown_reviews_refused_packages_and_different_existing_directories() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("downloaded");
    let user = temp.path().join("user");
    fs::create_dir(&user).unwrap();
    let package = write_package(&source, "notes", &["filesystem.read"], &["filesystem.read"]);
    let shown = review_applet_package(&package, &source).unwrap();

    let mut altered: AppletReview = shown.clone();
    altered.exports.clear();
    let error = format!(
        "{:#}",
        install_applet_package(&package, &source, &user, &altered).unwrap_err()
    );
    assert!(
        error.contains("differ from the review that was shown"),
        "{error}"
    );
    assert!(!user.join("notes").exists());

    let installed = install_applet_package(&package, &source, &user, &shown).unwrap();
    fs::write(installed.directory.join(LIBRARY), "edited\n").unwrap();
    let error = format!(
        "{:#}",
        install_applet_package(&package, &source, &user, &shown).unwrap_err()
    );
    assert!(error.contains("not byte-identical"), "{error}");
    assert_eq!(
        fs::read_to_string(installed.directory.join(LIBRARY)).unwrap(),
        "edited\n"
    );

    let undeclared_source = temp.path().join("undeclared");
    let undeclared = write_package(&undeclared_source, "undeclared", &[], &["process.exec"]);
    let refused = review_applet_package(&undeclared, &undeclared_source).unwrap();
    let error = format!(
        "{:#}",
        install_applet_package(&undeclared, &undeclared_source, &user, &refused).unwrap_err()
    );
    assert!(error.contains("is refused"), "{error}");
    assert!(!user.join("undeclared").exists());
    assert!(!user.join("undeclared.lock").exists());
}

#[test]
fn remove_refuses_local_edits_and_otherwise_deletes_only_listed_files() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("downloaded");
    let user = temp.path().join("user");
    fs::create_dir(&user).unwrap();
    let package = write_package(&source, "notes", &["filesystem.read"], &["filesystem.read"]);
    let shown = review_applet_package(&package, &source).unwrap();
    let installed = install_applet_package(&package, &source, &user, &shown).unwrap();

    let library = installed.directory.join(LIBRARY);
    fs::write(&library, "edited\n").unwrap();
    let error = format!("{:#}", remove_applet_package("notes", &user).unwrap_err());
    assert!(error.contains("changed since install"), "{error}");
    assert_eq!(fs::read_to_string(&library).unwrap(), "edited\n");
    assert!(installed.lock_path.is_file());

    fs::write(&library, SOURCE).unwrap();
    let removal = remove_applet_package("notes", &user).unwrap();
    assert_eq!(
        removal.removed_files,
        [APPLET_PACKAGE_MANIFEST_FILE, LIBRARY]
    );
    assert!(removal.directory_removed);
    assert!(!installed.directory.exists());
    assert!(!installed.lock_path.exists());
}

#[test]
fn remove_keeps_files_the_user_added() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("downloaded");
    let user = temp.path().join("user");
    fs::create_dir(&user).unwrap();
    let package = write_package(&source, "notes", &[], &[]);
    let shown = review_applet_package(&package, &source).unwrap();
    let installed = install_applet_package(&package, &source, &user, &shown).unwrap();
    fs::write(installed.directory.join("notes.txt"), "mine").unwrap();

    let removal = remove_applet_package("notes", &user).unwrap();
    assert!(!removal.directory_removed);
    assert!(!installed.directory.join(LIBRARY).exists());
    assert_eq!(
        fs::read_to_string(installed.directory.join("notes.txt")).unwrap(),
        "mine"
    );
    assert!(!installed.lock_path.exists());
}

#[test]
fn remove_refuses_a_missing_listed_file_and_unsafe_names() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("downloaded");
    let user = temp.path().join("user");
    fs::create_dir(&user).unwrap();
    let package = write_package(&source, "notes", &[], &[]);
    let shown = review_applet_package(&package, &source).unwrap();
    let installed = install_applet_package(&package, &source, &user, &shown).unwrap();
    fs::remove_file(installed.directory.join(APPLET_PACKAGE_MANIFEST_FILE)).unwrap();

    let error = format!("{:#}", remove_applet_package("notes", &user).unwrap_err());
    assert!(error.contains("missing or not a regular file"), "{error}");
    assert!(installed.directory.join(LIBRARY).is_file());
    assert!(installed.lock_path.is_file());

    assert!(remove_applet_package("../notes", &user).is_err());
    assert!(remove_applet_package("notes/x", &user).is_err());
}
