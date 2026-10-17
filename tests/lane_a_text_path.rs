//! In-process text and path builders: default-if-empty, replace, strip prefix or
//! suffix, join, case mapping and path join. Portable: no process is spawned.

use flow::applets::{execute, AppletError};
use serde_json::{json, Map, Value};
use std::path::Path;

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn call(operation: &str, arguments: Value) -> Result<Value, AppletError> {
    execute(operation, &args(arguments), &[])
}

#[test]
fn default_if_empty_only_replaces_zero_length_text() {
    assert_eq!(
        call(
            "text.default_if_empty",
            json!({"text": "", "default": "fallback"})
        )
        .unwrap(),
        json!("fallback")
    );
    assert_eq!(
        call(
            "text.default_if_empty",
            json!({"text": " ", "default": "fallback"})
        )
        .unwrap(),
        json!(" ")
    );
    assert_eq!(
        call(
            "text.default_if_empty",
            json!({"text": "set", "default": "fallback"})
        )
        .unwrap(),
        json!("set")
    );
    assert!(call("text.default_if_empty", json!({"text": "x"})).is_err());
}

#[test]
fn replace_is_literal_non_overlapping_and_left_to_right() {
    assert_eq!(
        call(
            "text.replace",
            json!({"text": "a-b-c", "old": "-", "new": "+"})
        )
        .unwrap(),
        json!("a+b+c")
    );
    assert_eq!(
        call(
            "text.replace",
            json!({"text": "aaaa", "old": "aa", "new": "b"})
        )
        .unwrap(),
        json!("bb")
    );
    assert_eq!(
        call(
            "text.replace",
            json!({"text": "a.b", "old": ".", "new": ""})
        )
        .unwrap(),
        json!("ab")
    );
    assert_eq!(
        call(
            "text.replace",
            json!({"text": "keep", "old": "zz", "new": "y"})
        )
        .unwrap(),
        json!("keep")
    );
    assert_eq!(
        call(
            "text.replace",
            json!({"text": "雪雪", "old": "雪", "new": "snow"})
        )
        .unwrap(),
        json!("snowsnow")
    );
    // Regex and glob syntax are plain characters here.
    assert_eq!(
        call(
            "text.replace",
            json!({"text": "a*b", "old": "*", "new": "$1"})
        )
        .unwrap(),
        json!("a$1b")
    );
    let error = call("text.replace", json!({"text": "x", "old": "", "new": "y"})).unwrap_err();
    assert_eq!(error.exit_status(), 2);
}

#[test]
fn strip_prefix_and_suffix_leave_other_text_unchanged() {
    assert_eq!(
        call(
            "text.strip_prefix",
            json!({"text": "foo.txt", "prefix": "foo."})
        )
        .unwrap(),
        json!("txt")
    );
    assert_eq!(
        call(
            "text.strip_prefix",
            json!({"text": "foo.txt", "prefix": "bar"})
        )
        .unwrap(),
        json!("foo.txt")
    );
    assert_eq!(
        call("text.strip_prefix", json!({"text": "foo", "prefix": ""})).unwrap(),
        json!("foo")
    );
    assert_eq!(
        call(
            "text.strip_suffix",
            json!({"text": "a.tar.gz", "suffix": ".gz"})
        )
        .unwrap(),
        json!("a.tar")
    );
    assert_eq!(
        call(
            "text.strip_suffix",
            json!({"text": "a.tar", "suffix": ".gz"})
        )
        .unwrap(),
        json!("a.tar")
    );
    // Only one occurrence is removed, from the matching end.
    assert_eq!(
        call("text.strip_suffix", json!({"text": "xx", "suffix": "x"})).unwrap(),
        json!("x")
    );
}

#[test]
fn case_mapping_uses_unicode_defaults() {
    assert_eq!(
        call("text.lower", json!({"text": "ÀBC"})).unwrap(),
        json!("àbc")
    );
    assert_eq!(
        call("text.upper", json!({"text": "abc"})).unwrap(),
        json!("ABC")
    );
    assert_eq!(
        call("text.upper", json!({"text": "straße"})).unwrap(),
        json!("STRASSE")
    );
    assert_eq!(call("text.lower", json!({"text": ""})).unwrap(), json!(""));
}

#[test]
fn text_join_concatenates_parts_with_an_optional_separator() {
    assert_eq!(
        call(
            "text.join",
            json!({"parts": ["a", "b", "c"], "separator": "/"})
        )
        .unwrap(),
        json!("a/b/c")
    );
    assert_eq!(
        call("text.join", json!({"parts": ["a", "b"]})).unwrap(),
        json!("ab")
    );
    assert_eq!(call("text.join", json!({"parts": []})).unwrap(), json!(""));
    assert!(call("text.join", json!({"parts": [1]})).is_err());
}

#[test]
fn path_join_adds_platform_separators_and_skips_empty_segments() {
    let expected = Path::new("config").join("omarchy").join("theme.toml");
    assert_eq!(
        call(
            "path.join",
            json!({"parts": ["config", "omarchy", "theme.toml"]})
        )
        .unwrap(),
        json!(expected.to_str().unwrap())
    );
    assert_eq!(
        call("path.join", json!({"parts": ["", "config", ""]})).unwrap(),
        json!("config")
    );
    assert_eq!(call("path.join", json!({"parts": []})).unwrap(), json!(""));
    // As with Path::join, an absolute segment replaces the path built so far.
    let absolute = Path::new("config").join("/etc").join("x");
    assert_eq!(
        call("path.join", json!({"parts": ["config", "/etc", "x"]})).unwrap(),
        json!(absolute.to_str().unwrap())
    );
    assert!(call("path.join", json!({"parts": ["a\u{0}b"]})).is_err());
    assert!(call("path.join", json!({"parts": "a"})).is_err());
}
