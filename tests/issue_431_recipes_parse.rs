//! Issue #431: every recipe in docs/recipes must parse, and docs/README.md must link each one.

use std::fs;
use std::path::PathBuf;

fn repo_path(parts: &[&str]) -> PathBuf {
    parts
        .iter()
        .fold(PathBuf::from(env!("CARGO_MANIFEST_DIR")), |path, part| {
            path.join(part)
        })
}

fn recipe_paths() -> Vec<PathBuf> {
    let directory = repo_path(&["docs", "recipes"]);
    let mut paths: Vec<PathBuf> = fs::read_dir(&directory)
        .expect("read docs/recipes")
        .map(|entry| entry.expect("read recipe directory entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("program"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn every_recipe_parses() {
    let paths = recipe_paths();
    assert!(
        paths.len() >= 10,
        "expected at least 10 recipes, found {}",
        paths.len()
    );
    for path in &paths {
        let source = fs::read_to_string(path).expect("read recipe");
        if let Err(error) = flow::parser::parse(&source) {
            panic!("{} does not parse: {error:?}", path.display());
        }
    }
}

#[test]
fn docs_index_links_every_recipe() {
    let index = fs::read_to_string(repo_path(&["docs", "README.md"])).expect("read docs/README.md");
    for path in recipe_paths() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("recipe file name");
        let link = format!("recipes/{name}");
        assert!(index.contains(&link), "docs/README.md does not link {link}");
    }
}
