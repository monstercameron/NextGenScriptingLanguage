#![cfg(unix)]

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    manifest_dir: PathBuf,
    manifest_catalog: PathBuf,
    workflow: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let manifest_dir = temp.path().join("custom-manifests");
        fs::create_dir(&manifest_dir).unwrap();
        let manifest: Value =
            serde_json::from_slice(include_bytes!("../manifests/printf.json")).unwrap();
        fs::write(
            manifest_dir.join("printf.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let manifest_catalog = temp.path().join("custom.manifest-catalog");
        fs::write(
            &manifest_catalog,
            serde_json::to_vec(&json!({
                "format": "execute.manifest-catalog",
                "version": 1,
                "manifests": [manifest]
            }))
            .unwrap(),
        )
        .unwrap();
        let workflow = temp.path().join("workflow.program");
        Self {
            temp,
            manifest_dir,
            manifest_catalog,
            workflow,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(self.temp.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn path(path: &Path) -> &str {
        path.to_str().unwrap()
    }
}

#[test]
fn ordinary_run_and_applet_metadata_match_directory_mode() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.workflow,
        "run printf with message \"packed catalog parity\"\n",
    )
    .unwrap();

    let directory_run = fixture.cli(&[
        "run",
        Fixture::path(&fixture.workflow),
        "--manifest-dir",
        Fixture::path(&fixture.manifest_dir),
    ]);
    let packed_run = fixture.cli(&[
        "run",
        Fixture::path(&fixture.workflow),
        "--manifest-catalog",
        Fixture::path(&fixture.manifest_catalog),
    ]);
    assert!(
        directory_run.status.success(),
        "{}",
        String::from_utf8_lossy(&directory_run.stderr)
    );
    assert!(
        packed_run.status.success(),
        "{}",
        String::from_utf8_lossy(&packed_run.stderr)
    );
    assert_eq!(directory_run.stdout, b"packed catalog parity\n");
    assert_eq!(packed_run.stdout, directory_run.stdout);

    let directory_list = fixture.cli(&[
        "applets",
        "list",
        "--manifest-dir",
        Fixture::path(&fixture.manifest_dir),
    ]);
    let packed_list = fixture.cli(&[
        "applets",
        "list",
        "--manifest-catalog",
        Fixture::path(&fixture.manifest_catalog),
    ]);
    assert!(directory_list.status.success());
    assert!(packed_list.status.success());
    let directory_list: Value = serde_json::from_slice(&directory_list.stdout).unwrap();
    let packed_list: Value = serde_json::from_slice(&packed_list.stdout).unwrap();
    assert_eq!(packed_list, directory_list);

    let directory_inspect = fixture.cli(&[
        "applets",
        "inspect",
        "printf",
        "--manifest-dir",
        Fixture::path(&fixture.manifest_dir),
    ]);
    let packed_inspect = fixture.cli(&[
        "applets",
        "inspect",
        "printf",
        "--manifest-catalog",
        Fixture::path(&fixture.manifest_catalog),
    ]);
    assert!(directory_inspect.status.success());
    assert!(packed_inspect.status.success());
    let directory_inspect: Value = serde_json::from_slice(&directory_inspect.stdout).unwrap();
    let packed_inspect: Value = serde_json::from_slice(&packed_inspect.stdout).unwrap();
    assert_eq!(packed_inspect, directory_inspect);
}

#[test]
fn no_selector_never_searches_the_working_directory() {
    let fixture = Fixture::new();
    let local = fixture.temp.path().join("manifests");
    fs::create_dir(&local).unwrap();
    let local_manifest: Value = json!({
        "name": "local.default",
        "binary": "printf",
        "arguments": []
    });
    fs::write(
        local.join("local.json"),
        serde_json::to_vec(&local_manifest).unwrap(),
    )
    .unwrap();
    let packed_only: Value = json!({
        "name": "packed.only",
        "binary": "printf",
        "arguments": []
    });
    fs::write(
        &fixture.manifest_catalog,
        serde_json::to_vec(&json!({
            "format": "execute.manifest-catalog",
            "version": 1,
            "manifests": [packed_only]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = fixture.cli(&["applets", "list"]);
    assert!(output.status.success());
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    // The working directory is never searched, so its manifests/ cannot shadow the
    // bundled applets.
    assert!(listed["applets"]
        .as_array()
        .unwrap()
        .iter()
        .all(|applet| applet["name"] != "local.default"));
}

#[test]
fn selectors_conflict_for_ordinary_applets_service_and_live_serve() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.workflow,
        "every 5 minutes\n  then encode \"x\" as json\n",
    )
    .unwrap();

    let socket = fixture.temp.path().join("live.sock");
    let cases: &[&[&str]] = &[
        &[
            "check",
            Fixture::path(&fixture.workflow),
            "--manifest-dir",
            Fixture::path(&fixture.manifest_dir),
            "--manifest-catalog",
            Fixture::path(&fixture.manifest_catalog),
        ],
        &[
            "applets",
            "list",
            "--manifest-dir",
            Fixture::path(&fixture.manifest_dir),
            "--manifest-catalog",
            Fixture::path(&fixture.manifest_catalog),
        ],
        &[
            "service",
            "generate",
            "systemd",
            Fixture::path(&fixture.workflow),
            "--manifest-dir",
            Fixture::path(&fixture.manifest_dir),
            "--manifest-catalog",
            Fixture::path(&fixture.manifest_catalog),
        ],
        &[
            "live",
            "serve",
            Fixture::path(&fixture.workflow),
            "--socket",
            Fixture::path(&socket),
            "--manifest-dir",
            Fixture::path(&fixture.manifest_dir),
            "--manifest-catalog",
            Fixture::path(&fixture.manifest_catalog),
        ],
    ];
    for args in cases {
        let output = fixture.cli(args);
        assert!(!output.status.success(), "{args:?}");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined.contains("--manifest-dir and --manifest-catalog are mutually exclusive"),
            "{args:?}: {combined}"
        );
    }
}

#[test]
fn live_serve_loads_the_explicit_packed_catalog_before_planning() {
    let fixture = Fixture::new();
    fs::write(&fixture.workflow, "not a valid workflow\n").unwrap();
    let invalid_catalog = fixture.temp.path().join("invalid.manifest-catalog");
    fs::write(
        &invalid_catalog,
        r#"{"format":"execute.manifest-catalog","version":1,"manifests":[{"name":"","binary":"printf","arguments":[]}]}"#,
    )
    .unwrap();
    let socket = fixture.temp.path().join("live-invalid.sock");

    let output = fixture.cli(&[
        "live",
        "serve",
        Fixture::path(&fixture.workflow),
        "--socket",
        Fixture::path(&socket),
        "--manifest-catalog",
        Fixture::path(&invalid_catalog),
    ]);
    assert!(!output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    let message = response["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("invalid manifest catalog entry 0"),
        "{message}"
    );
}

#[test]
fn generated_service_uses_canonical_explicit_catalog_path() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.workflow,
        "every 5 minutes\n  then encode \"x\" as json\n",
    )
    .unwrap();
    let relative_catalog = fixture
        .manifest_catalog
        .strip_prefix(fixture.temp.path())
        .unwrap();
    let relative_catalog = relative_catalog.to_str().unwrap();

    let output = fixture.cli(&[
        "service",
        "generate",
        "systemd",
        Fixture::path(&fixture.workflow),
        "--manifest-catalog",
        relative_catalog,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = String::from_utf8(output.stdout).unwrap();
    let canonical_catalog = fs::canonicalize(&fixture.manifest_catalog).unwrap();
    assert!(generated.contains("--manifest-catalog"));
    assert!(generated.contains(canonical_catalog.to_str().unwrap()));
    assert!(!generated.contains("--manifest-dir"));
}
