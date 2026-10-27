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

#[test]
fn default_candidate_selection_keeps_order_and_skips_missing_roots() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("first");
    let selected = temp.path().join("selected");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(first.join("choice.json"), manifest("lane.first")).unwrap();
    fs::write(selected.join("choice.json"), manifest("lane.selected")).unwrap();

    let registry = Registry::load_default_from_candidates(
        &[temp.path().join("missing"), first.clone(), selected],
        flow::manifest::ManifestCheck::Lazy,
    )
    .unwrap();

    assert!(registry.resolve("lane.first").is_ok());
    assert!(registry.resolve("lane.selected").is_err());
    assert_eq!(
        registry.module_root().unwrap(),
        fs::canonicalize(first).unwrap()
    );
}

#[test]
fn bundled_candidate_registries_keep_verification_state_root_specific() {
    let temp = tempdir().unwrap();
    let good = temp.path().join("good");
    let changed = temp.path().join("changed");
    fs::create_dir(&good).unwrap();
    fs::create_dir(&changed).unwrap();
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    for entry in fs::read_dir(shipped).unwrap() {
        let entry = entry.unwrap();
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            fs::copy(entry.path(), good.join(entry.file_name())).unwrap();
            fs::copy(entry.path(), changed.join(entry.file_name())).unwrap();
        }
    }
    let awk = changed.join("awk.json");
    let mut altered = fs::read_to_string(&awk).unwrap();
    altered.push(' ');
    fs::write(awk, altered).unwrap();

    let valid = Registry::load_default_from_candidates(
        std::slice::from_ref(&good),
        flow::manifest::ManifestCheck::Lazy,
    )
    .unwrap();
    let invalid = Registry::load_default_from_candidates(
        std::slice::from_ref(&changed),
        flow::manifest::ManifestCheck::Lazy,
    )
    .unwrap();
    let names = (0..8)
        .map(|_| {
            let valid = valid.clone();
            std::thread::spawn(move || valid.resolve("awk").unwrap().name.clone())
        })
        .collect::<Vec<_>>();

    assert!(names
        .into_iter()
        .all(|thread| thread.join().unwrap() == "awk"));
    let error = format!("{:#}", invalid.resolve("awk").unwrap_err());
    assert!(error.contains(&fs::canonicalize(changed).unwrap().display().to_string()));
    assert!(error.contains("differs from the bundled copy"));
}

#[cfg(unix)]
#[test]
fn lazy_bundled_verification_refuses_a_manifest_replaced_by_a_symlink() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    for entry in fs::read_dir(shipped).unwrap() {
        let entry = entry.unwrap();
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            fs::copy(entry.path(), root.join(entry.file_name())).unwrap();
        }
    }
    let registry = Registry::load_default_from_candidates(
        std::slice::from_ref(&root),
        flow::manifest::ManifestCheck::Lazy,
    )
    .unwrap();
    let outside = temp.path().join("outside.json");
    fs::copy(root.join("awk.json"), &outside).unwrap();
    fs::remove_file(root.join("awk.json")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("awk.json")).unwrap();

    let error = format!("{:#}", registry.resolve("awk").unwrap_err());
    assert!(error.contains("differs from the bundled copy"), "{error}");
}

#[test]
fn implicit_default_loader_rejects_regular_files_but_explicit_load_still_works() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("manifest.json");
    fs::write(&file, manifest("lane.file")).unwrap();
    let error = Registry::load_default_directory_only(&file, flow::manifest::ManifestCheck::Lazy)
        .err()
        .expect("implicit defaults must be directories");
    assert!(format!("{error:#}").contains("not a directory"));
    assert!(Registry::load(&file).unwrap().resolve("lane.file").is_ok());
}
