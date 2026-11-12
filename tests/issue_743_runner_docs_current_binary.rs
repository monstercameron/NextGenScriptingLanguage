use std::fs;
use std::path::Path;

fn read(relative: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative))
        .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"))
}

#[test]
fn ladder_runner_does_not_default_to_historical_binary() {
    let text = read("examples/ladder/run.py");
    assert!(
        !text.contains("PROJECT / 'bin/execute'"),
        "run.py must not default to the historical bin/execute"
    );
    assert!(
        text.contains("PROJECT / 'target' / 'release' / name"),
        "run.py must default to the release build"
    );
    assert!(
        text.contains("executable not found"),
        "run.py must explain a missing build"
    );
}

#[test]
fn ladder_readme_uses_current_build_only() {
    let text = read("examples/ladder/README.md");
    assert!(
        text.contains("cargo build --locked --release"),
        "ladder README must state the build prerequisite"
    );
    assert_eq!(
        text.matches("bin/execute").count(),
        1,
        "only the historical-artifact note may name bin/execute"
    );
    assert!(!text.contains("--binary bin/execute"));
}

#[test]
fn guides_invoke_release_build_not_historical_binary() {
    for relative in ["docs/github-stars.md", "docs/top-cpu.md"] {
        let text = read(relative);
        assert!(
            !text.contains("bin/execute"),
            "{relative} still invokes the historical bin/execute"
        );
        assert!(
            text.contains("cargo build --locked --release"),
            "{relative} must state the build prerequisite"
        );
        assert!(
            text.contains("target/release/execute"),
            "{relative} must invoke target/release/execute"
        );
    }
}
