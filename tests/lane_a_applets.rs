//! In-process applets for file tests, globs and line lists. Portable: fixtures
//! use owned temporary directories and avoid names Windows cannot store.

use flow::applets::{
    execute, execute_in_cwd, AppletError, AppletErrorKind, ExecutionBudgetExceeded,
    ExecutionContext,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn test_kind(path: &Path, kind: &str) -> Result<Value, AppletError> {
    execute("fs.test", &args(json!({"path": path, "kind": kind})), &[])
}

fn glob_in(
    cwd: &Path,
    pattern: &str,
    include_hidden: Option<bool>,
) -> Result<Vec<String>, AppletError> {
    let mut arguments = json!({"pattern": pattern});
    if let Some(value) = include_hidden {
        arguments["include_hidden"] = json!(value);
    }
    let value = execute_in_cwd("fs.glob", &args(arguments), &[], cwd)?;
    Ok(value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect())
}

fn lines(text: &str) -> Result<Value, AppletError> {
    execute("text.lines", &args(json!({"text": text})), &[])
}

fn split(text: &str, extra: Value) -> Result<Value, AppletError> {
    let mut arguments = json!({"text": text});
    for (key, value) in extra.as_object().unwrap() {
        arguments[key] = value.clone();
    }
    execute("text.split", &args(arguments), &[])
}

#[test]
fn fs_test_answers_booleans_and_treats_absence_as_false() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("plain.txt");
    fs::write(&file, b"data").unwrap();
    let empty = temp.path().join("empty.txt");
    fs::write(&empty, b"").unwrap();
    let directory = temp.path().join("dir");
    fs::create_dir(&directory).unwrap();
    let missing = temp.path().join("missing");

    assert_eq!(test_kind(&file, "exists").unwrap(), json!(true));
    assert_eq!(test_kind(&file, "file").unwrap(), json!(true));
    assert_eq!(test_kind(&file, "directory").unwrap(), json!(false));
    assert_eq!(test_kind(&file, "empty").unwrap(), json!(false));
    assert_eq!(test_kind(&empty, "empty").unwrap(), json!(true));
    assert_eq!(test_kind(&directory, "directory").unwrap(), json!(true));
    assert_eq!(test_kind(&directory, "file").unwrap(), json!(false));
    assert_eq!(test_kind(&directory, "readable").unwrap(), json!(true));
    assert_eq!(test_kind(&directory, "empty").unwrap(), json!(true));
    assert_eq!(test_kind(&file, "readable").unwrap(), json!(true));
    assert_eq!(test_kind(&file, "writable").unwrap(), json!(true));
    for kind in [
        "exists",
        "file",
        "directory",
        "symlink",
        "readable",
        "writable",
        "executable",
        "empty",
    ] {
        assert_eq!(test_kind(&missing, kind).unwrap(), json!(false), "{kind}");
    }
}

#[test]
fn fs_test_rejects_unknown_kinds_and_empty_paths_before_touching_disk() {
    let error = test_kind(Path::new("anything"), "regular").unwrap_err();
    assert_eq!(error.exit_status(), 2);
    assert!(error.message.contains("args.kind"));
    assert!(execute("fs.test", &args(json!({"path": "", "kind": "exists"})), &[]).is_err());
}

#[test]
fn fs_test_resolves_relative_paths_against_the_given_directory() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("here.txt"), b"x").unwrap();
    let value = execute_in_cwd(
        "fs.test",
        &args(json!({"path": "here.txt", "kind": "file"})),
        &[],
        temp.path(),
    )
    .unwrap();
    assert_eq!(value, json!(true));
}

#[cfg(unix)]
#[test]
fn fs_test_symlink_inspects_the_link_and_other_kinds_follow_it() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    fs::write(&target, b"x").unwrap();
    let live = temp.path().join("live");
    symlink(&target, &live).unwrap();
    let dangling = temp.path().join("dangling");
    symlink(temp.path().join("nowhere"), &dangling).unwrap();

    assert_eq!(test_kind(&live, "symlink").unwrap(), json!(true));
    assert_eq!(test_kind(&live, "file").unwrap(), json!(true));
    assert_eq!(test_kind(&dangling, "symlink").unwrap(), json!(true));
    assert_eq!(test_kind(&dangling, "exists").unwrap(), json!(false));
    assert_eq!(test_kind(&target, "symlink").unwrap(), json!(false));
}

#[test]
fn fs_glob_sorts_matches_and_returns_empty_lists_for_no_match() {
    let temp = tempfile::tempdir().unwrap();
    // Uppercase sorts before lowercase in byte (code-point) order.
    for name in ["b.txt", "a.txt", "c.md", "Z.txt"] {
        fs::write(temp.path().join(name), b"").unwrap();
    }
    assert_eq!(
        glob_in(temp.path(), "*.txt", None).unwrap(),
        vec!["Z.txt", "a.txt", "b.txt"]
    );
    assert_eq!(
        glob_in(temp.path(), "*.none", None).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        glob_in(&temp.path().join("absent"), "*", None).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        glob_in(temp.path(), "a.txt/*", None).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(glob_in(temp.path(), "a.txt", None).unwrap(), vec!["a.txt"]);
    assert_eq!(
        glob_in(temp.path(), "zz", None).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn fs_glob_skips_dotfiles_unless_asked() {
    let temp = tempfile::tempdir().unwrap();
    for name in [".hidden", "visible", ".also.txt"] {
        fs::write(temp.path().join(name), b"").unwrap();
    }
    assert_eq!(glob_in(temp.path(), "*", None).unwrap(), vec!["visible"]);
    assert_eq!(
        glob_in(temp.path(), "*", Some(false)).unwrap(),
        vec!["visible"]
    );
    assert_eq!(
        glob_in(temp.path(), "*", Some(true)).unwrap(),
        vec![".also.txt", ".hidden", "visible"]
    );
    assert_eq!(
        glob_in(temp.path(), ".*", None).unwrap(),
        vec![".also.txt", ".hidden"]
    );
    assert_eq!(
        glob_in(temp.path(), ".hidden", None).unwrap(),
        vec![".hidden"]
    );
}

#[test]
fn fs_glob_supports_question_marks_sets_and_special_characters() {
    let temp = tempfile::tempdir().unwrap();
    for name in [
        "a1.log",
        "a2.log",
        "ab.log",
        "with space.txt",
        "a[1].txt",
        "café.md",
    ] {
        fs::write(temp.path().join(name), b"").unwrap();
    }
    assert_eq!(
        glob_in(temp.path(), "a?.log", None).unwrap(),
        vec!["a1.log", "a2.log", "ab.log"]
    );
    assert_eq!(
        glob_in(temp.path(), "a[0-9].log", None).unwrap(),
        vec!["a1.log", "a2.log"]
    );
    assert_eq!(
        glob_in(temp.path(), "a[!0-9]*", None).unwrap(),
        vec!["a[1].txt", "ab.log"]
    );
    // Bracket sets make a literal bracket expressible without an escape syntax.
    assert_eq!(
        glob_in(temp.path(), "a[[]1].txt", None).unwrap(),
        vec!["a[1].txt"]
    );
    assert_eq!(
        glob_in(temp.path(), "with *", None).unwrap(),
        vec!["with space.txt"]
    );
    assert_eq!(
        glob_in(temp.path(), "caf?.md", None).unwrap(),
        vec!["café.md"]
    );
}

#[test]
fn fs_glob_matches_directories_only_with_a_trailing_separator() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("sub")).unwrap();
    fs::write(temp.path().join("file"), b"").unwrap();
    let separator = std::path::MAIN_SEPARATOR;
    assert_eq!(
        glob_in(temp.path(), "*/", None).unwrap(),
        vec![format!("sub{separator}")]
    );
}

#[test]
fn fs_glob_double_star_descends_real_directories_only() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("top.txt"), b"").unwrap();
    fs::create_dir_all(temp.path().join("a/b")).unwrap();
    fs::write(temp.path().join("a/mid.txt"), b"").unwrap();
    fs::write(temp.path().join("a/b/deep.txt"), b"").unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();
    fs::write(temp.path().join(".git/config.txt"), b"").unwrap();

    let separator = std::path::MAIN_SEPARATOR;
    assert_eq!(
        glob_in(temp.path(), "**/*.txt", None).unwrap(),
        vec![
            format!("a{separator}b{separator}deep.txt"),
            format!("a{separator}mid.txt"),
            "top.txt".to_owned(),
        ]
    );
    // Directories `a` and `a/b`, the three files, and no hidden `.git` entries.
    assert_eq!(glob_in(temp.path(), "**/*", None).unwrap().len(), 5);
    assert!(glob_in(temp.path(), "**", None).is_err());
}

#[cfg(unix)]
#[test]
fn fs_glob_double_star_does_not_follow_symbolic_links() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("inside.txt"), b"").unwrap();
    symlink(&real, temp.path().join("link")).unwrap();
    assert_eq!(
        glob_in(temp.path(), "**/*.txt", None).unwrap(),
        vec!["real/inside.txt".to_owned()]
    );
}

#[test]
fn fs_glob_treats_absolute_patterns_as_absolute_results() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("only.txt"), b"").unwrap();
    let pattern = temp.path().join("*.txt");
    let results = glob_in(
        Path::new("/nonexistent-cwd"),
        pattern.to_str().unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(
        results,
        vec![temp.path().join("only.txt").to_str().unwrap().to_owned()]
    );
}

#[test]
fn fs_glob_rejects_invalid_patterns_and_arguments() {
    let temp = tempfile::tempdir().unwrap();
    assert!(glob_in(temp.path(), "", None).is_err());
    assert!(glob_in(temp.path(), "file[0-9", None).is_err());
    assert!(glob_in(temp.path(), "x[z-a]", None).is_err());
    assert!(glob_in(temp.path(), "trailing/**", None).is_err());
    assert!(glob_in(temp.path(), &"x".repeat(5000), None).is_err());
    assert!(glob_in(temp.path(), "a\0b", None).is_err());
    let error = execute(
        "fs.glob",
        &args(json!({"pattern": "*", "include_hidden": "yes"})),
        &[],
    )
    .unwrap_err();
    assert_eq!(error.exit_status(), 2);
}

#[test]
fn fs_glob_and_lines_observe_cancellation_at_checkpoints() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..300 {
        fs::write(temp.path().join(format!("f{index:03}.txt")), b"").unwrap();
    }
    let cancelled =
        || -> Result<(), ExecutionBudgetExceeded> { Err(ExecutionBudgetExceeded::Cancelled) };
    let context = ExecutionContext::new(&cancelled);
    let error = flow::applets::execute_with_context(
        "fs.glob",
        &args(json!({"pattern": "*.txt"})),
        &[],
        temp.path(),
        &context,
    )
    .unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Cancelled);

    let error = flow::applets::execute_with_context(
        "text.lines",
        &args(json!({"text": "a\n".repeat(1000)})),
        &[],
        temp.path(),
        &context,
    )
    .unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Cancelled);
}

#[test]
fn text_lines_strips_one_terminator_and_one_carriage_return_per_line() {
    assert_eq!(lines("").unwrap(), json!([]));
    assert_eq!(lines("\n").unwrap(), json!([""]));
    assert_eq!(lines("a").unwrap(), json!(["a"]));
    assert_eq!(lines("a\n").unwrap(), json!(["a"]));
    assert_eq!(lines("a\n\n").unwrap(), json!(["a", ""]));
    assert_eq!(lines("a\r\nb\r\n").unwrap(), json!(["a", "b"]));
    assert_eq!(lines("a\r\r\n").unwrap(), json!(["a\r"]));
    assert_eq!(lines("a\r").unwrap(), json!(["a"]));
    assert_eq!(lines("  spaced  \n").unwrap(), json!(["  spaced  "]));
    assert_eq!(lines("é\n雪\n").unwrap(), json!(["é", "雪"]));
}

#[test]
fn text_lines_reads_stdin_when_text_is_absent_and_caps_items() {
    let value = execute("text.lines", &Map::new(), b"one\ntwo\n").unwrap();
    assert_eq!(value, json!(["one", "two"]));
    let error = execute("text.lines", &Map::new(), &[0xff]).unwrap_err();
    assert_eq!(error.exit_status(), 2);
    let oversized = "\n".repeat(100_001);
    assert!(lines(&oversized).is_err());
    assert_eq!(
        lines(&"\n".repeat(100_000))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        100_000
    );
}

#[test]
fn text_split_uses_a_literal_separator_and_terminator_rule() {
    assert_eq!(
        split("a,b,", json!({"separator": ","})).unwrap(),
        json!(["a", "b"])
    );
    assert_eq!(
        split("a,,b", json!({"separator": ","})).unwrap(),
        json!(["a", "", "b"])
    );
    assert_eq!(split(",", json!({"separator": ","})).unwrap(), json!([""]));
    assert_eq!(split("", json!({"separator": ","})).unwrap(), json!([]));
    assert_eq!(
        split("x::y::", json!({"separator": "::"})).unwrap(),
        json!(["x", "y"])
    );
    assert_eq!(
        split("a.b", json!({"separator": "*"})).unwrap(),
        json!(["a.b"])
    );
}

#[test]
fn text_split_nul_mode_handles_find_print0_style_output() {
    assert_eq!(
        split("one two\0three\0", json!({"nul": true})).unwrap(),
        json!(["one two", "three"])
    );
    assert_eq!(
        split("a\nb\0", json!({"nul": true})).unwrap(),
        json!(["a\nb"])
    );
    assert_eq!(
        split("a\0\0", json!({"nul": true})).unwrap(),
        json!(["a", ""])
    );
    assert_eq!(
        split("a\0b", json!({"nul": false, "separator": "\n"})).unwrap(),
        json!(["a\0b"])
    );
}

#[test]
fn text_split_requires_exactly_one_separator_mode() {
    assert!(split("a,b", json!({})).is_err());
    assert!(split("a,b", json!({"separator": ",", "nul": true})).is_err());
    assert!(split("a,b", json!({"separator": ""})).is_err());
    assert!(split("a,b", json!({"separator": 1})).is_err());
    assert!(split("a,b", json!({"nul": "yes"})).is_err());
    let value = execute(
        "text.split",
        &args(json!({"separator": ","})),
        b"left,right",
    )
    .unwrap();
    assert_eq!(value, json!(["left", "right"]));
}

#[test]
fn missing_directories_and_paths_are_empty_or_false_not_errors() {
    let missing_cwd = std::env::temp_dir().join("lane-a-missing-cwd-does-not-exist");
    assert!(glob_in(&missing_cwd, "*", None).unwrap().is_empty());
    assert_eq!(
        test_kind(Path::new("/definitely/not/here"), "exists").unwrap(),
        json!(false)
    );
}
