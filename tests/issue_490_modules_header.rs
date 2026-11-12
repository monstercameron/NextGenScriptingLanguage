//! Issue #490: a program names its module catalog in a leading `# execute-modules:` header.

use flow::manifest::{
    ensure_no_shadowed_applets, execute_modules_header, resolve_execute_modules_dir,
};
use std::fs;
use std::path::Path;

#[test]
fn header_names_a_relative_catalog() {
    let source = "# execute-modules: ./catalog\naccept payload as json\n";
    let path = execute_modules_header(source).unwrap();
    assert_eq!(path.as_deref(), Some(Path::new("./catalog")));
}

#[test]
fn program_without_header_names_no_catalog() {
    let source = "accept payload as json\n";
    assert_eq!(execute_modules_header(source).unwrap(), None);
}

#[test]
fn header_after_statements_is_not_a_header() {
    let source = "accept payload as json\n# execute-modules: ./catalog\n";
    assert_eq!(execute_modules_header(source).unwrap(), None);
}

#[test]
fn duplicate_headers_are_refused() {
    let source = "# execute-modules: ./a\n# execute-modules: ./b\n";
    assert!(execute_modules_header(source).is_err());
}

#[test]
fn absolute_and_parent_paths_are_refused_in_the_header() {
    assert!(execute_modules_header("# execute-modules: ../outside\n").is_err());
    let absolute = if cfg!(windows) {
        "C:\\outside"
    } else {
        "/outside"
    };
    let source = format!("# execute-modules: {absolute}\n");
    assert!(execute_modules_header(&source).is_err());
}

#[test]
fn catalog_outside_the_program_tree_is_refused() {
    let base = tempfile::tempdir().unwrap();
    let tree = base.path().join("tree");
    let outside = base.path().join("outside");
    fs::create_dir_all(&tree).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let error = resolve_execute_modules_dir(&tree, Path::new("../outside")).unwrap_err();
    assert!(error
        .to_string()
        .contains("outside the program's directory tree"));
}

#[test]
fn catalog_inside_the_program_tree_resolves() {
    let base = tempfile::tempdir().unwrap();
    let catalog = base.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let resolved = resolve_execute_modules_dir(base.path(), Path::new("catalog")).unwrap();
    assert!(resolved.is_dir());
    assert!(resolved.ends_with("catalog"));
}

#[test]
fn catalog_applet_shadowing_a_shipped_applet_is_refused() {
    let error = ensure_no_shadowed_applets(&["wait", "echo"], &["examples.records.report", "echo"])
        .unwrap_err();
    assert!(error.to_string().contains("shadows shipped applet"));
    assert!(error.to_string().contains("echo"));
}

#[test]
fn catalog_with_new_names_is_accepted() {
    ensure_no_shadowed_applets(&["wait", "echo"], &["examples.records.report"]).unwrap();
}

#[test]
fn example_caller_checks_without_a_manifest_flag() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "examples/modules/caller.program"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("unknown applet"), "{stderr}");
    assert!(output.status.success(), "{stderr}");
}

#[test]
fn program_header_pointing_outside_its_tree_is_refused_by_check() {
    let base = tempfile::tempdir().unwrap();
    let tree = base.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::create_dir_all(base.path().join("outside")).unwrap();
    let program = tree.join("caller.program");
    fs::write(
        &program,
        "# execute-modules: ../outside\naccept payload as json\n",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg(&program)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("must not contain `..`"), "{stderr}");
}
