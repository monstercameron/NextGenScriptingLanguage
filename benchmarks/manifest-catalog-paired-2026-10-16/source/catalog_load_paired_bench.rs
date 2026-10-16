//! Ignored same-binary comparison of directory and packed catalog library loads.

use flow::manifest::Registry;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::tempdir;

const EXPECTED_MANIFESTS: usize = 75;
const WARMUP_PAIRS: usize = 5;
const MEASURED_PAIRS: usize = 48;
const LOADS_PER_SIDE: usize = 5;

#[derive(Serialize)]
struct SourceInputHash {
    relative_path: String,
    sha256: String,
}

#[derive(Serialize)]
struct BatchDuration {
    duration_ns: u128,
    loaded_manifest_count: usize,
}

#[derive(Serialize)]
struct PairedSample {
    pair_index: usize,
    first: &'static str,
    second: &'static str,
    directory: BatchDuration,
    packed_catalog: BatchDuration,
}

#[derive(Serialize)]
struct PairedReport {
    schema: &'static str,
    comparison: &'static str,
    claim_scope: &'static str,
    package_version: &'static str,
    candidate_test_binary: String,
    candidate_test_binary_sha256: String,
    source_manifest_count: usize,
    source_inputs: Vec<SourceInputHash>,
    serialized_manifest_specs_sha256: String,
    catalog_data_sha256: String,
    loads_per_side_per_duration: usize,
    warmup_pairs: Vec<PairedSample>,
    measured_pairs: Vec<PairedSample>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn source_manifest_paths(directory: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(directory)
        .expect("reading source manifest directory")
        .map(|entry| entry.expect("reading source manifest entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn prepare_fixture(
    source_directory: &Path,
) -> (tempfile::TempDir, PathBuf, Vec<SourceInputHash>, String) {
    let temporary = tempdir().expect("creating owned catalog fixture directory");
    let fixture_directory = temporary.path().join("manifests");
    fs::create_dir(&fixture_directory).expect("creating fixture manifest directory");
    let paths = source_manifest_paths(source_directory);
    assert_eq!(paths.len(), EXPECTED_MANIFESTS);

    let mut source_inputs = Vec::with_capacity(paths.len());
    let mut catalog_data =
        String::from(r#"{"format":"execute.manifest-catalog","version":1,"manifests":["#);
    for (index, path) in paths.iter().enumerate() {
        let file_type = fs::symlink_metadata(path)
            .expect("reading source manifest type")
            .file_type();
        assert!(
            file_type.is_file(),
            "source manifest must be a regular file: {path:?}"
        );

        let bytes = fs::read(path).expect("reading source manifest bytes");
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("source manifest filename must be UTF-8");
        fs::write(fixture_directory.join(file_name), &bytes)
            .expect("copying source manifest into owned fixture");
        source_inputs.push(SourceInputHash {
            relative_path: format!("manifests/{file_name}"),
            sha256: sha256_hex(&bytes),
        });

        if index > 0 {
            catalog_data.push(',');
        }
        catalog_data
            .push_str(std::str::from_utf8(&bytes).expect("source manifest must be valid UTF-8"));
    }
    catalog_data.push_str("]}\n");

    let catalog_path = fixture_directory.join("manifests.catalog");
    assert_ne!(
        catalog_path.extension().and_then(|ext| ext.to_str()),
        Some("json")
    );
    fs::write(&catalog_path, catalog_data.as_bytes()).expect("writing packed catalog fixture");
    let catalog_data_sha256 = sha256_hex(catalog_data.as_bytes());
    (
        temporary,
        fixture_directory,
        source_inputs,
        catalog_data_sha256,
    )
}

fn measure_directory(directory: &Path) -> BatchDuration {
    let started = Instant::now();
    let mut loaded_manifest_count = 0;
    for _ in 0..LOADS_PER_SIDE {
        let registry = Registry::load(black_box(directory)).expect("loading directory registry");
        loaded_manifest_count += black_box(registry.manifests().len());
        black_box(registry);
    }
    BatchDuration {
        duration_ns: started.elapsed().as_nanos(),
        loaded_manifest_count,
    }
}

fn measure_packed_catalog(catalog: &Path) -> BatchDuration {
    let started = Instant::now();
    let mut loaded_manifest_count = 0;
    for _ in 0..LOADS_PER_SIDE {
        let registry = Registry::load_catalog(black_box(catalog)).expect("loading packed registry");
        loaded_manifest_count += black_box(registry.manifests().len());
        black_box(registry);
    }
    BatchDuration {
        duration_ns: started.elapsed().as_nanos(),
        loaded_manifest_count,
    }
}

fn measure_pair(
    pair_index: usize,
    directory_first: bool,
    directory: &Path,
    catalog: &Path,
) -> PairedSample {
    let (first, second, directory_sample, catalog_sample) = if directory_first {
        let directory_sample = measure_directory(directory);
        let catalog_sample = measure_packed_catalog(catalog);
        (
            "directory",
            "packed_catalog",
            directory_sample,
            catalog_sample,
        )
    } else {
        let catalog_sample = measure_packed_catalog(catalog);
        let directory_sample = measure_directory(directory);
        (
            "packed_catalog",
            "directory",
            directory_sample,
            catalog_sample,
        )
    };
    PairedSample {
        pair_index,
        first,
        second,
        directory: directory_sample,
        packed_catalog: catalog_sample,
    }
}

fn assert_batch_counts(pairs: &[PairedSample]) {
    let expected = EXPECTED_MANIFESTS * LOADS_PER_SIDE;
    for pair in pairs {
        assert_eq!(pair.directory.loaded_manifest_count, expected);
        assert_eq!(pair.packed_catalog.loaded_manifest_count, expected);
    }
}

#[test]
#[ignore = "explicit release paired representation benchmark; writes a fresh JSON report"]
fn registry_directory_vs_packed_catalog_48_pairs() {
    let report_path = PathBuf::from(
        std::env::var_os("EXECUTE_MANIFEST_CATALOG_PAIR_REPORT")
            .expect("set EXECUTE_MANIFEST_CATALOG_PAIR_REPORT to a fresh absolute JSON path"),
    );
    assert!(
        report_path.is_absolute(),
        "paired report path must be absolute"
    );
    assert_eq!(
        report_path
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("json"),
        "paired report path must use a .json extension"
    );
    assert!(
        matches!(
            fs::symlink_metadata(&report_path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        "paired report path must not exist already: {report_path:?}"
    );
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent).expect("creating paired report directory");
    }

    let source_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let (temporary, fixture_directory, source_inputs, catalog_data_sha256) =
        prepare_fixture(&source_directory);
    let catalog_path = fixture_directory.join("manifests.catalog");
    let report_fixture_path = temporary.path().to_path_buf();
    assert!(!report_path.starts_with(&report_fixture_path));

    let directory_registry = Registry::load(&fixture_directory).expect("loading directory oracle");
    let catalog_registry = Registry::load_catalog(&catalog_path).expect("loading packed oracle");
    let directory_specs = serde_json::to_vec(directory_registry.manifests())
        .expect("serializing directory manifest specs");
    let catalog_specs = serde_json::to_vec(catalog_registry.manifests())
        .expect("serializing packed manifest specs");
    assert_eq!(directory_specs, catalog_specs);
    assert_eq!(directory_registry.manifests().len(), EXPECTED_MANIFESTS);
    let serialized_manifest_specs_sha256 = sha256_hex(&directory_specs);
    drop(directory_registry);
    drop(catalog_registry);

    let candidate_binary = std::env::current_exe().expect("reading candidate test binary path");
    assert!(candidate_binary.is_absolute());
    let candidate_binary_sha256 =
        sha256_hex(&fs::read(&candidate_binary).expect("reading candidate test binary bytes"));

    let mut warmup_pairs = Vec::with_capacity(WARMUP_PAIRS);
    for pair_index in 0..WARMUP_PAIRS {
        warmup_pairs.push(measure_pair(
            pair_index,
            pair_index % 2 == 0,
            &fixture_directory,
            &catalog_path,
        ));
    }
    let mut measured_pairs = Vec::with_capacity(MEASURED_PAIRS);
    for pair_index in 0..MEASURED_PAIRS {
        measured_pairs.push(measure_pair(
            pair_index,
            pair_index % 2 == 0,
            &fixture_directory,
            &catalog_path,
        ));
    }
    assert_batch_counts(&warmup_pairs);
    assert_batch_counts(&measured_pairs);

    let report = PairedReport {
        schema: "execute.registry-load-representation-pairs/1",
        comparison: "same-binary directory Registry::load vs explicit Registry::load_catalog",
        claim_scope:
            "physical library representation comparison; not unchanged-default CLI startup",
        package_version: env!("CARGO_PKG_VERSION"),
        candidate_test_binary: candidate_binary.to_string_lossy().into_owned(),
        candidate_test_binary_sha256: candidate_binary_sha256,
        source_manifest_count: EXPECTED_MANIFESTS,
        source_inputs,
        serialized_manifest_specs_sha256,
        catalog_data_sha256,
        loads_per_side_per_duration: LOADS_PER_SIDE,
        warmup_pairs,
        measured_pairs,
    };
    let report_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .expect("creating fresh paired JSON report");
    serde_json::to_writer_pretty(report_file, &report).expect("writing paired JSON report");

    temporary.close().expect("removing owned catalog fixture");
    assert!(
        !report_fixture_path.exists(),
        "catalog fixture must be removed"
    );
}
