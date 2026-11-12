//! #328: `*.packages` set files parse to package names with comments and blank lines removed,
//! and `set_delta` reports only the names that differ from the installed list. Pure: no process
//! is spawned and no database is read.

use flow::packages::arch::PackageSpec;
use flow::packages::{parse_package_set, set_delta, PackagesError};

const FIXTURE: &str = "\
# Base system packages
base
linux

# Editors
neovim
   git
# trailing blank line follows

";

#[test]
fn set_file_drops_comments_and_blank_lines_and_keeps_order() {
    let specs = parse_package_set(FIXTURE).expect("fixture names are valid Arch names");
    let names: Vec<&str> = specs.iter().map(PackageSpec::name).collect();
    assert_eq!(names, ["base", "linux", "neovim", "git"]);
}

#[test]
fn set_file_with_only_comments_is_empty() {
    let specs = parse_package_set("# nothing here\n\n   \n").expect("no names is not an error");
    assert!(specs.is_empty());
}

#[test]
fn set_file_with_a_malformed_name_is_refused_whole() {
    let err =
        parse_package_set("base\nNOT A NAME\n").expect_err("uppercase and spaces are refused");
    assert!(matches!(err, PackagesError::InvalidSpec { .. }));
}

#[test]
fn delta_reports_only_names_that_differ() {
    let desired = parse_package_set(FIXTURE).expect("fixture parses");
    let installed = ["base", "bash", "git", "zsh"];
    let delta = set_delta(&desired, &installed);
    assert_eq!(delta.install, ["linux", "neovim"]);
    assert_eq!(delta.remove, ["bash", "zsh"]);
}

#[test]
fn delta_against_an_installed_set_that_already_matches_is_empty() {
    let desired = parse_package_set("base\ngit\n").expect("fixture parses");
    let delta = set_delta(&desired, &["git", "base"]);
    assert!(delta.install.is_empty());
    assert!(delta.remove.is_empty());
}

#[test]
fn delta_lists_a_duplicate_desired_name_once() {
    let desired = parse_package_set("git\ngit\n").expect("fixture parses");
    let delta = set_delta(&desired, &[]);
    assert_eq!(delta.install, ["git"]);
}
