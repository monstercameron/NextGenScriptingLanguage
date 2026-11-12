use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, process::Command};

#[path = "common/portable_shell.rs"]
mod portable_shell;

// The applet rewrites its declared dependency A -> B, reads B, then restores A
// before it exits. The content fingerprint is A on both sides of the run, so
// only the timestamp stamp reveals the excursion. Neither run may store, and no
// later A-keyed run may replay the output computed while B was present.
#[test]
fn dependency_restored_after_mid_run_excursion_is_not_stored_or_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let manifests = root.join("manifests");
    fs::create_dir(&manifests).unwrap();

    fs::write(root.join("state.txt"), "before").unwrap();
    let helper = root.join("excursion-dependency");
    fs::write(
        &helper,
        "#!/bin/sh\nset -eu\nprintf changed > state.txt\nvalue=$(cat state.txt)\nprintf before > state.txt\nprintf '%s\\n' \"$value\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    // Direct execution needs a shebang with an executable bit, which Windows
    // cannot provide; there the helper runs through the platform shell.
    let (binary, arguments) = if cfg!(windows) {
        (
            portable_shell::shell_binary("sh"),
            serde_json::json!([{"literal": helper.to_str().unwrap()}]),
        )
    } else {
        (helper.to_string_lossy().into_owned(), serde_json::json!([]))
    };
    fs::write(
        manifests.join("excursion.json"),
        serde_json::to_vec(&json!({
            "name": "excursion",
            "binary": binary,
            "cacheable": true,
            "cache_dependencies": ["state.txt"],
            "arguments": arguments
        }))
        .unwrap(),
    )
    .unwrap();

    let workflow = root.join("excursion.program");
    fs::write(
        &workflow,
        "run excursion and cache\nrun excursion and cache\n",
    )
    .unwrap();
    let cache = root.join("cache");
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root)
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--cache-dir")
        .arg(&cache)
        .arg("--trace")
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"changed\n");
    let trace = String::from_utf8_lossy(&result.stderr);
    assert!(
        !trace.contains("\"event\":\"cache_hit\""),
        "a run replayed output computed during a dependency excursion: {trace}"
    );
    assert_eq!(
        trace.matches("\"event\":\"cache_store\"").count(),
        0,
        "output computed during a dependency excursion was stored: {trace}"
    );
    assert_eq!(fs::read(root.join("state.txt")).unwrap(), b"before");
}
