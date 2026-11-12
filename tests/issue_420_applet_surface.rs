//! Issue #420: the applet surface (applet names, parameter names and parameter
//! types read from the manifests in `manifests/`) is pinned by a snapshot in
//! `tests/fixtures/applet-surface.json`. Removing an applet or a parameter, or
//! changing a parameter's type, fails the check. Additions fail the exact
//! comparison until the snapshot is refreshed with EXECUTE_UPDATE_APPLET_SURFACE=1.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// applet name -> parameter name -> parameter signature (type, with items when given).
type Surface = BTreeMap<String, BTreeMap<String, String>>;

fn project() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn snapshot_path() -> PathBuf {
    project().join("tests/fixtures/applet-surface.json")
}

fn parameter_signature(parameter: &serde_json::Value) -> String {
    let kind = parameter
        .get("type")
        .and_then(|kind| kind.as_str())
        .unwrap_or("unspecified");
    match parameter.get("items") {
        Some(items) => {
            let items = items
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| items.to_string());
            format!("{kind}<{items}>")
        }
        None => kind.to_string(),
    }
}

fn surface_from_dir(dir: &Path) -> Surface {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("listing manifests")
        .map(|entry| entry.expect("reading a manifests entry").path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("json")
        })
        .collect();
    paths.sort();
    let mut surface = Surface::new();
    for path in paths {
        let file = path.display().to_string();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{file}: {error}"));
        let manifest: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|error| panic!("{file}: {error}"));
        let applet = manifest
            .get("name")
            .and_then(|name| name.as_str())
            .unwrap_or_else(|| panic!("{file}: manifest has no string name"));
        let mut parameters = BTreeMap::new();
        if let Some(declared) = manifest
            .get("parameters")
            .and_then(|value| value.as_object())
        {
            for (name, definition) in declared {
                parameters.insert(name.clone(), parameter_signature(definition));
            }
        }
        assert!(
            surface.insert(applet.to_string(), parameters).is_none(),
            "{file}: applet `{applet}` is declared by more than one manifest"
        );
    }
    surface
}

/// Breaking differences: applets or parameters the snapshot has that `current` lacks,
/// and parameters whose signature changed.
fn breaking_differences(snapshot: &Surface, current: &Surface) -> Vec<String> {
    let mut problems = Vec::new();
    for (applet, parameters) in snapshot {
        let Some(now) = current.get(applet) else {
            problems.push(format!("applet `{applet}` was removed"));
            continue;
        };
        for (parameter, signature) in parameters {
            match now.get(parameter) {
                None => problems.push(format!(
                    "applet `{applet}` parameter `{parameter}` was removed"
                )),
                Some(now_signature) if now_signature != signature => problems.push(format!(
                    "applet `{applet}` parameter `{parameter}` changed from {signature} to {now_signature}"
                )),
                Some(_) => {}
            }
        }
    }
    problems
}

fn read_snapshot() -> Surface {
    let path = snapshot_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; generate it with EXECUTE_UPDATE_APPLET_SURFACE=1",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("applet-surface.json is a surface snapshot")
}

#[test]
fn applet_surface_matches_snapshot() {
    let current = surface_from_dir(&project().join("manifests"));
    assert!(
        current.len() > 100,
        "expected over 100 applets in manifests/, found {}",
        current.len()
    );
    if std::env::var_os("EXECUTE_UPDATE_APPLET_SURFACE").is_some() {
        let mut text = serde_json::to_string_pretty(&current).expect("serializing the surface");
        text.push('\n');
        std::fs::write(snapshot_path(), text).expect("writing applet-surface.json");
        return;
    }
    let snapshot = read_snapshot();
    let breaking = breaking_differences(&snapshot, &current);
    assert!(
        breaking.is_empty(),
        "breaking applet surface change (needs a Breaking entry in CHANGELOG.md and a \
         deliberate snapshot update):\n{}",
        breaking.join("\n")
    );
    assert!(
        current == snapshot,
        "applet surface gained entries not in tests/fixtures/applet-surface.json; \
         refresh it with EXECUTE_UPDATE_APPLET_SURFACE=1"
    );
}

#[test]
fn removed_parameter_fails_the_surface_check() {
    // The baseline is the unedited manifests, not the snapshot file, so this test does not
    // race the snapshot test when the snapshot is being regenerated.
    let baseline = surface_from_dir(&project().join("manifests"));
    let copy = tempfile::tempdir().expect("creating a temporary manifest directory");
    for entry in std::fs::read_dir(project().join("manifests")).expect("listing manifests") {
        let path = entry.expect("reading a manifests entry").path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            let target = copy
                .path()
                .join(path.file_name().expect("manifest file name"));
            std::fs::copy(&path, &target).expect("copying a manifest");
        }
    }
    let cat = copy.path().join("cat.json");
    let text = std::fs::read_to_string(&cat).expect("reading the copied cat.json");
    let mut manifest: serde_json::Value = serde_json::from_str(&text).expect("cat.json parses");
    let removed = manifest
        .get_mut("parameters")
        .and_then(|parameters| parameters.as_object_mut())
        .expect("cat.json declares parameters")
        .remove("path");
    assert!(removed.is_some(), "cat.json declares a `path` parameter");
    std::fs::write(
        &cat,
        serde_json::to_string_pretty(&manifest).expect("serializing the edited cat.json"),
    )
    .expect("writing the edited cat.json");

    let problems = breaking_differences(&baseline, &surface_from_dir(copy.path()));
    assert!(
        problems
            .iter()
            .any(|problem| problem == "applet `cat` parameter `path` was removed"),
        "expected the removal of cat's `path` to be reported, got {problems:?}"
    );
}
