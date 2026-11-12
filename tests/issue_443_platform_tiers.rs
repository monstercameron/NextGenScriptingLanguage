//! Issue #443: the platform tier document names the three tiers and the README links it.

const PLATFORMS: &str = include_str!("../docs/platforms.md");
const README: &str = include_str!("../README.md");

#[test]
fn platform_tiers_are_documented() {
    for heading in ["Tier 1: Linux", "Tier 2: macOS", "Tier 3: Windows"] {
        assert!(PLATFORMS.contains(heading), "missing heading: {heading}");
    }
}

#[test]
fn readme_links_platform_tiers() {
    assert!(README.contains("(docs/platforms.md)"));
}
