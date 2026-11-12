// Issue 427: the Omarchy quickstart exists, covers the six steps, is registered with the
// documentation link checker and is linked from the README.
use std::fs;
use std::path::Path;

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn omarchy_quickstart_covers_the_six_steps() {
    let guide = read("docs/omarchy-quickstart.md");
    for heading in [
        "## 1. Install",
        "## 2. Write a toggle",
        "## 3. Bind it in Hyprland",
        "## 4. Test it with a fake hyprctl",
        "## 5. Run a hook when the theme changes",
        "## 6. Run a monitor listener as a user service",
        "## Not yet available",
    ] {
        assert!(guide.contains(heading), "missing heading: {heading}");
    }
}

#[test]
fn omarchy_quickstart_is_registered_and_linked() {
    let checker = read("tools/check_docs.py");
    assert!(
        checker.contains("\"docs/omarchy-quickstart.md\""),
        "tools/check_docs.py does not list the quickstart"
    );
    let readme = read("README.md");
    assert!(
        readme.contains("(docs/omarchy-quickstart.md)"),
        "README.md does not link the quickstart"
    );
}

/// The first fenced `text` block in section 2 is the program the guide asks the reader to
/// save as `toggle-floating.program`.
fn toggle_program_source(guide: &str) -> String {
    let section = guide
        .split("## 2. Write a toggle")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("section 2 of the quickstart");
    section
        .split("```text\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("a text block in section 2")
        .to_string()
}

fn bundled_registry(directory: &Path) -> flow::manifest::Registry {
    let index = flow::bundled_manifests::index().expect("bundled index");
    for (position, entry) in index.entries().iter().enumerate() {
        let body = index.body(position).expect("bundled manifest body");
        fs::write(directory.join(entry.file()), body).expect("write bundled manifest");
    }
    flow::manifest::Registry::from_bundled(directory).expect("bundled registry")
}

#[test]
fn omarchy_quickstart_toggle_program_validates_against_bundled_registry() {
    let guide = read("docs/omarchy-quickstart.md");
    let source = toggle_program_source(&guide);
    assert!(
        source.contains("run hypr.dispatch with arguments [\"togglefloating\"]"),
        "unexpected quickstart program:\n{source}"
    );
    let directory = tempfile::tempdir().expect("temp directory");
    let registry = bundled_registry(directory.path());
    let program = flow::parser::parse(&source).expect("quickstart toggle parses");
    flow::runtime::ExecutionPlan::prepare(&program, &registry)
        .expect("quickstart toggle validates against the bundled registry");

    // The guide says a dispatcher that is not on the allowlist is refused before hyprctl starts.
    let refused = source.replace("togglefloating", "keyword");
    let program = flow::parser::parse(&refused).expect("refused variant parses");
    assert!(
        flow::runtime::ExecutionPlan::prepare(&program, &registry).is_err(),
        "a dispatcher outside the allowlist must fail validation"
    );
}
