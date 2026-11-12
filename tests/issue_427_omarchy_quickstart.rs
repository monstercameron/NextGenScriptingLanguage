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
