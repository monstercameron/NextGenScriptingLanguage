use flow::ast::Value;
use flow::contracts::AppletBackend;
use flow::manifest::Registry;
use flow::modules::load_program_module;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const CATALOG_FORMAT: &str = "execute.manifest-catalog";

fn write_catalog(path: &Path, manifests: &[String]) {
    let body = manifests.join(",");
    fs::write(
        path,
        format!(r#"{{"format":"{CATALOG_FORMAT}","version":1,"manifests":[{body}]}}"#),
    )
    .unwrap();
}

fn write_sorted_catalog_from_directory(directory: &Path, output: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let manifests = paths
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>();
    write_catalog(output, &manifests);
    paths
}

fn module_manifest() -> String {
    json!({
        "name": "catalog.module",
        "binary": "@program-module",
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "module.program",
                "entry": "describe",
                "interface_version": 1
            },
            "lifecycle": { "kind": "oneshot" },
            "output": { "type": "text", "encoding": "json" }
        }
    })
    .to_string()
}

#[test]
fn packed_catalog_matches_every_manifest_spec_and_representative_argv() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    // Count the shipped specs from disk so that adding a manifest does not
    // require editing this test.
    let expected = std::fs::read_dir(&root)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .count();
    let temp = tempdir().unwrap();
    let catalog_path = temp.path().join("manifests.catalog");
    let source_paths = write_sorted_catalog_from_directory(&root, &catalog_path);
    let directory = Registry::load(&root).unwrap();
    let packed = Registry::load_catalog(&catalog_path).unwrap();

    assert!(expected > 0, "manifests directory has no JSON specs");
    assert_eq!(source_paths.len(), expected);
    assert_eq!(directory.manifests().unwrap().len(), expected);
    assert_eq!(packed.manifests().unwrap().len(), expected);
    assert_eq!(
        directory
            .manifests()
            .unwrap()
            .iter()
            .map(|manifest| manifest.name.as_str())
            .collect::<Vec<_>>(),
        packed
            .manifests()
            .unwrap()
            .iter()
            .map(|manifest| manifest.name.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        serde_json::to_vec(&directory.manifests().unwrap()).unwrap(),
        serde_json::to_vec(&packed.manifests().unwrap()).unwrap()
    );
    assert!(directory
        .manifests()
        .unwrap()
        .iter()
        .all(|manifest| manifest.execution_backend() != AppletBackend::ProgramModule));

    let values = BTreeMap::from([(
        "message".to_owned(),
        Value::String("catalog equivalence".to_owned()),
    )]);
    let directory_argv = directory
        .resolve("printf")
        .unwrap()
        .build_arguments(&values)
        .unwrap();
    let packed_argv = packed
        .resolve("printf")
        .unwrap()
        .build_arguments(&values)
        .unwrap();
    assert_eq!(directory_argv, packed_argv);
    assert_eq!(directory_argv, ["%s\\n", "catalog equivalence"]);
}

#[test]
fn packed_entries_validate_in_array_order_and_report_entry_context() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("invalid.catalog");
    let first = r#"{"name":"","binary":"fixture","arguments":[]}"#.to_owned();
    let second = r#"{"name":"later","binary":"fixture","arguments":[]}"#.to_owned();
    write_catalog(&path, &[first, second]);

    let error = Registry::load_catalog(&path)
        .err()
        .expect("invalid first entry must fail before later entries are considered");
    let message = format!("{error:#}");
    assert!(
        message.contains("invalid manifest catalog entry 0"),
        "{message}"
    );
    assert!(
        message.contains("manifest name and binary must be nonempty"),
        "{message}"
    );
    assert!(!message.contains("entry 1"), "{message}");
}

#[test]
fn catalog_syntax_is_parsed_before_manifest_semantics() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("syntax-before-semantics.catalog");
    fs::write(
        &path,
        r#"{"format":"execute.manifest-catalog","version":1,"manifests":[{"name":"","binary":"fixture","arguments":[]},{"name":]}"#,
    )
    .unwrap();

    let error = Registry::load_catalog(&path)
        .err()
        .expect("later envelope syntax error must fail");
    let message = format!("{error:#}");
    assert!(message.contains("parsing manifest catalog"), "{message}");
    assert!(
        !message.contains("invalid manifest catalog entry 0"),
        "{message}"
    );
}

#[test]
fn packed_duplicate_alias_error_identifies_the_later_entry() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("duplicate.catalog");
    let first = json!({
        "name": "first",
        "binary": "fixture",
        "phrases": ["shared phrase"],
        "arguments": []
    })
    .to_string();
    let second = json!({
        "name": "second",
        "binary": "fixture",
        "phrases": ["shared phrase"],
        "arguments": []
    })
    .to_string();
    write_catalog(&path, &[first, second]);

    let error = Registry::load_catalog(&path)
        .err()
        .expect("duplicate aliases must be rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("invalid manifest catalog entry 1"),
        "{message}"
    );
    assert!(
        message.contains(
            "duplicate manifest name or phrase \"shared phrase\" (used by \"first\" and \"second\")"
        ),
        "{message}"
    );
}

#[test]
fn packed_envelope_rejects_malformed_unknown_and_unsupported_headers() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("header.catalog");
    for (source, expected) in [
        ("{", "parsing manifest catalog"),
        (
            r#"{"format":"other","version":1,"manifests":[]}"#,
            "unsupported manifest catalog format",
        ),
        (
            r#"{"format":"execute.manifest-catalog","version":2,"manifests":[]}"#,
            "unsupported manifest catalog version",
        ),
        (
            r#"{"format":"execute.manifest-catalog","version":1,"manifests":[],"extra":true}"#,
            "unknown field `extra`",
        ),
    ] {
        fs::write(&path, source).unwrap();
        let error = Registry::load_catalog(&path)
            .err()
            .expect("invalid catalog header must fail");
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }

    fs::write(
        &path,
        r#"{"format":"execute.manifest-\u0063atalog","version":1,"manifests":[{"name":"escaped","binary":"fixture","arguments":[]}]}"#,
    )
    .unwrap();
    assert_eq!(
        Registry::load_catalog(&path)
            .unwrap()
            .manifests()
            .unwrap()
            .len(),
        1
    );

    fs::write(&path, r#"{"version":1,"manifests":[]}"#).unwrap();
    let error = Registry::load_catalog(&path)
        .err()
        .expect("missing required header fields must fail");
    assert!(
        format!("{error:#}").contains("missing field `format`"),
        "{error:#}"
    );

    fs::write(
        &path,
        r#"{"format":"execute.manifest-catalog","version":1,"manifests":[]}"#,
    )
    .unwrap();
    let error = Registry::load_catalog(&path)
        .err()
        .expect("empty catalogs must not hide a packaging error");
    assert!(format!("{error:#}").contains("contains no manifests"));
}

#[test]
fn catalog_module_origin_keeps_relative_program_source_under_same_root() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("applets");
    fs::create_dir_all(&root).unwrap();
    let source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    fs::write(root.join("module.program"), source).unwrap();
    let manifest_path = root.join("module.json");
    fs::write(&manifest_path, module_manifest()).unwrap();
    let catalog_path = root.join("catalog.manifest-catalog");
    write_sorted_catalog_from_directory(&root, &catalog_path);

    let directory = Registry::load(&root).unwrap();
    let packed = Registry::load_catalog(&catalog_path).unwrap();
    let expected_root = fs::canonicalize(&root).unwrap();
    let expected_source = fs::canonicalize(root.join("module.program")).unwrap();
    assert_eq!(directory.module_root(), Some(expected_root.as_path()));
    assert_eq!(packed.module_root(), Some(expected_root.as_path()));
    assert_eq!(
        directory.resolve("catalog.module").unwrap().origin(),
        Some(fs::canonicalize(&manifest_path).unwrap().as_path())
    );
    assert_eq!(
        packed.resolve("catalog.module").unwrap().origin(),
        Some(fs::canonicalize(&catalog_path).unwrap().as_path())
    );
    #[cfg(unix)]
    {
        let alias_path = root.join("catalog-link");
        std::os::unix::fs::symlink(&catalog_path, &alias_path).unwrap();
        let via_alias = Registry::load_catalog(&alias_path).unwrap();
        assert_eq!(via_alias.module_root(), Some(expected_root.as_path()));
        assert_eq!(
            via_alias.resolve("catalog.module").unwrap().origin(),
            Some(fs::canonicalize(&catalog_path).unwrap().as_path())
        );
    }

    let load_source = |registry: &Registry| {
        let manifest = registry.resolve("catalog.module").unwrap();
        let reference = manifest
            .execution_contract()
            .unwrap()
            .program
            .as_ref()
            .unwrap();
        load_program_module(
            reference,
            manifest.origin().unwrap(),
            registry.module_root().unwrap(),
        )
        .unwrap()
    };
    let directory_module = load_source(&directory);
    let packed_module = load_source(&packed);
    assert_eq!(directory_module.source_path, expected_source);
    assert_eq!(packed_module.source_path, expected_source);
    assert_eq!(directory_module.source_sha256, packed_module.source_sha256);
    assert_eq!(
        fs::read(&directory_module.source_path).unwrap(),
        source.as_bytes()
    );
}

#[test]
fn legacy_directory_and_single_manifest_loaders_keep_their_existing_shape() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("legacy");
    fs::create_dir_all(&root).unwrap();
    let alpha = root.join("a.json");
    let beta = root.join("b.json");
    fs::write(
        &alpha,
        r#"{"name":"alpha","binary":"fixture","arguments":[]}"#,
    )
    .unwrap();
    fs::write(
        &beta,
        r#"{"name":"beta","binary":"fixture","arguments":[]}"#,
    )
    .unwrap();
    fs::write(root.join("ignored.txt"), "not a manifest").unwrap();

    let directory = Registry::load(&root).unwrap();
    let single = Registry::load(&alpha).unwrap();
    let expected_root = fs::canonicalize(&root).unwrap();
    assert_eq!(directory.manifests().unwrap().len(), 2);
    assert_eq!(directory.module_root(), Some(expected_root.as_path()));
    assert_eq!(single.module_root(), Some(expected_root.as_path()));
    assert_eq!(
        directory
            .manifests()
            .unwrap()
            .iter()
            .map(|manifest| manifest.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    assert_eq!(single.manifests().unwrap().len(), 1);
    assert_eq!(single.resolve("alpha").unwrap().name, "alpha");
    assert_eq!(
        serde_json::to_vec(single.resolve("alpha").unwrap()).unwrap(),
        serde_json::to_vec(directory.resolve("alpha").unwrap()).unwrap()
    );
}

#[test]
fn catalog_header_does_not_accept_non_object_entries() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("wrong-entry.catalog");
    let source = json!({
        "format": CATALOG_FORMAT,
        "version": 1,
        "manifests": ["not an object"]
    });
    fs::write(&path, serde_json::to_string(&source).unwrap()).unwrap();
    let error = Registry::load_catalog(&path)
        .err()
        .expect("non-object entry must fail");
    assert!(format!("{error:#}").contains("invalid manifest catalog entry 0"));
}
