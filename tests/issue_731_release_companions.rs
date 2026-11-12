//! Issue #731: the release install guidance must name every companion binary
//! that current packages ship, and must agree with the generated release notes.

use std::fs;
use std::path::Path;

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        .replace("\r\n", "\n")
}

#[test]
fn archive_guidance_lists_all_three_binaries() {
    let docs = read("docs/releases.md");
    let line = docs
        .split("\n\n")
        .find(|paragraph| paragraph.contains("Every archive contains"))
        .expect("docs/releases.md must describe what every archive contains");
    assert!(
        line.contains("`execute-model`") && line.contains("`execute-applet`"),
        "archive paragraph must name execute-model and execute-applet: {line}"
    );
    assert!(
        docs.contains("Keep all three binaries together"),
        "docs must tell users to keep all three binaries together"
    );
}

#[test]
fn archive_guidance_keeps_the_rc4_exception() {
    let docs = read("docs/releases.md")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        docs.contains("`v0.9.0-rc.4` archives predate `execute-applet`"),
        "docs must keep the rc.4 archive distinction"
    );
}

#[test]
fn release_notes_match_the_docs() {
    let publish = read("tools/publish_release.py");
    assert!(
        publish.contains("keep execute, execute-model and execute-applet together"),
        "publish_release.py must name all three binaries for post-rc.4 notes"
    );
    assert!(
        publish.contains("keep execute and execute-model together"),
        "publish_release.py must keep the rc.4 wording"
    );
}
