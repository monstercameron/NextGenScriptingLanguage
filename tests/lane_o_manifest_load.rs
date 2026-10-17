//! Directory manifest loading: origins must stay canonical, and the first
//! invalid manifest in sorted order must still be the one reported.
use flow::manifest::Registry;
use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn manifest(name: &str) -> String {
    json!({
        "name": name,
        "binary": "fixture",
        "parameters": {},
        "arguments": []
    })
    .to_string()
}

#[test]
fn directory_origins_are_canonical_files_and_root_is_canonical_directory() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join("manifests");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("lane.a.json"), manifest("lane.a")).unwrap();
    fs::write(dir.join("lane.b.json"), manifest("lane.b")).unwrap();
    fs::write(dir.join("notes.txt"), "not a manifest").unwrap();

    let registry = Registry::load(&dir).unwrap();

    assert_eq!(registry.manifests().unwrap().len(), 2);
    assert_eq!(
        registry.module_root().unwrap(),
        fs::canonicalize(&dir).unwrap()
    );
    for name in ["lane.a", "lane.b"] {
        let origin = registry.resolve(name).unwrap().origin().unwrap();
        let file = dir.join(format!("{name}.json"));
        assert_eq!(origin, fs::canonicalize(&file).unwrap());
    }
}

#[test]
fn single_file_load_keeps_canonical_origin_and_parent_root() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("solo.json");
    fs::write(&file, manifest("lane.solo")).unwrap();

    let registry = Registry::load(&file).unwrap();

    let canonical = fs::canonicalize(&file).unwrap();
    assert_eq!(
        registry.resolve("lane.solo").unwrap().origin().unwrap(),
        canonical
    );
    assert_eq!(registry.module_root().unwrap(), canonical.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn symlinked_manifest_origin_is_its_canonical_target() {
    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    let dir = temp.path().join("manifests");
    fs::create_dir(&store).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::write(store.join("target.json"), manifest("lane.linked")).unwrap();
    std::os::unix::fs::symlink(store.join("target.json"), dir.join("link.json")).unwrap();

    let registry = Registry::load(&dir).unwrap();

    assert_eq!(
        registry.resolve("lane.linked").unwrap().origin().unwrap(),
        fs::canonicalize(store.join("target.json")).unwrap()
    );
}

#[test]
fn first_invalid_manifest_in_sorted_order_is_reported() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join("manifests");
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("a.json"),
        r#"{"name":"lane.bad","binary":"x","bogus":1}"#,
    )
    .unwrap();
    fs::write(
        dir.join("b.json"),
        r#"{"name":"lane.bad2","binary":"x","bogus":1}"#,
    )
    .unwrap();

    let error = Registry::load(&dir)
        .err()
        .expect("invalid manifests must fail");
    let rendered = format!("{error:#}");

    assert!(rendered.contains("invalid manifest"), "{rendered}");
    assert!(
        rendered.contains(&*dir.join("a.json").to_string_lossy()),
        "{rendered}"
    );
    assert!(!rendered.contains("b.json"), "{rendered}");
}

#[test]
fn empty_directory_is_still_rejected() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join("empty");
    fs::create_dir(&dir).unwrap();
    let error = Registry::load(Path::new(&dir))
        .err()
        .expect("empty directory must fail");
    assert!(format!("{error:#}").contains("no JSON manifests found"));
}
