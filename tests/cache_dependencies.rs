#![cfg(unix)]

use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn dependency_mutated_during_cached_call_is_not_stored_or_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let manifests = root.join("manifests");
    fs::create_dir(&manifests).unwrap();

    fs::write(root.join("state.txt"), "before").unwrap();
    let helper = root.join("mutate-dependency");
    fs::write(
        &helper,
        "#!/bin/sh\nset -eu\nvalue=$(cat state.txt)\nprintf '%s\\n' \"$value\"\nif [ \"$value\" = before ]; then printf after > state.txt; fi\n",
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        manifests.join("mutator.json"),
        serde_json::to_vec(&json!({
            "name": "mutator",
            "binary": "./mutate-dependency",
            "cacheable": true,
            "cache_dependencies": ["state.txt"],
            "arguments": []
        }))
        .unwrap(),
    )
    .unwrap();

    let workflow = root.join("cache-dependency.program");
    fs::write(&workflow, "run mutator and cache\nrun mutator and cache\n").unwrap();
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
    // The CLI presents the final top-level call's result, while the trace below
    // confirms both invocations ran instead of replaying the old output.
    assert_eq!(result.stdout, b"after\n");
    let trace = String::from_utf8_lossy(&result.stderr);
    assert_eq!(
        trace.matches("\"event\":\"cache_store\"").count(),
        1,
        "{trace}"
    );
    assert!(
        !trace.contains("\"event\":\"cache_hit\""),
        "second invocation replayed output from the changed dependency state: {trace}"
    );
    assert_eq!(fs::read(root.join("state.txt")).unwrap(), b"after");
}
