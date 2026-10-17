//! `text.render`: placeholder substitution for configuration templates. The
//! fixture reproduces the `{{ name }}` form of an Omarchy alacritty template; the
//! checked-in Omarchy file itself was not available to this lane.

use flow::applets::{execute, execute_in_cwd, AppletError};
use serde_json::{json, Map, Value};
use std::fs;

const ALACRITTY_TEMPLATE: &str = "[colors.primary]\nbackground = \"{{ background }}\"\nforeground = \"{{ foreground }}\"\n\n[colors.normal]\nblack = \"{{ color0 }}\"\n";

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn render(template: &str, values: Value) -> Result<Value, AppletError> {
    execute(
        "text.render",
        &args(json!({"template": template, "values": values})),
        &[],
    )
}

fn theme() -> Value {
    json!({"background": "#1e1e2e", "foreground": "#cdd6f4", "color0": "#45475a"})
}

#[test]
fn alacritty_style_template_reproduces_bytes_exactly() {
    let expected = "[colors.primary]\nbackground = \"#1e1e2e\"\nforeground = \"#cdd6f4\"\n\n[colors.normal]\nblack = \"#45475a\"\n";
    assert_eq!(
        render(ALACRITTY_TEMPLATE, theme()).unwrap(),
        json!(expected)
    );
}

#[test]
fn output_keeps_every_byte_outside_placeholders() {
    // No trailing newline is added or removed, and CRLF and spaces survive.
    let template = "a {{x}}\r\n\tb {{ x }}  c";
    assert_eq!(
        render(template, json!({"x": "1"})).unwrap(),
        json!("a 1\r\n\tb 1  c")
    );
    assert_eq!(render("", json!({})).unwrap(), json!(""));
    assert_eq!(
        render("no placeholders\n", json!({})).unwrap(),
        json!("no placeholders\n")
    );
}

#[test]
fn values_are_inserted_verbatim_and_never_rescanned() {
    let value = render("{{ a }}|{{ b }}", json!({"a": "{{ b }}", "b": "}} raw {{"})).unwrap();
    assert_eq!(value, json!("{{ b }}|}} raw {{"));
}

#[test]
fn numbers_and_booleans_render_in_canonical_form() {
    assert_eq!(
        render(
            "{{ n }} {{ f }} {{ t }} {{ z }}",
            json!({"n": 42, "f": 0.5, "t": true, "z": false})
        )
        .unwrap(),
        json!("42 0.5 true false")
    );
}

#[test]
fn missing_keys_and_non_scalar_values_fail() {
    let missing = render("x={{ absent }}", json!({})).unwrap_err();
    assert_eq!(missing.exit_status(), 2);
    assert!(missing.message.contains("absent"));
    assert!(render("{{ v }}", json!({"v": null})).is_err());
    assert!(render("{{ v }}", json!({"v": [1]})).is_err());
    assert!(render("{{ v }}", json!({"v": {"nested": 1}})).is_err());
}

#[test]
fn extra_keys_are_allowed_and_lookup_is_exact() {
    assert_eq!(
        render("{{ background }}", json!({"background": "x", "unused": 1})).unwrap(),
        json!("x")
    );
    assert!(render("{{ Background }}", json!({"background": "x"})).is_err());
}

#[test]
fn malformed_placeholders_fail_and_escapes_work() {
    assert!(render("{{ }}", json!({})).is_err());
    assert!(render("{{ a b }}", json!({"a": "x"})).is_err());
    assert!(render("{{ a", json!({"a": "x"})).is_err());
    assert!(render("{{ a }", json!({"a": "x"})).is_err());
    assert!(render("{{ ../x }}", json!({"../x": "x"})).is_err());
    assert_eq!(
        render("{{{{ a }}", json!({"a": "x"})).unwrap(),
        json!("{{ a }}")
    );
    assert_eq!(
        render("close }} here", json!({})).unwrap(),
        json!("close }} here")
    );
    let long_name = "n".repeat(129);
    assert!(render(
        &format!("{{{{ {long_name} }}}}"),
        json!({long_name.clone(): "x"})
    )
    .is_err());
}

#[test]
fn exactly_one_template_source_and_a_values_object_are_required() {
    let both = execute(
        "text.render",
        &args(json!({"template": "a", "template_path": "b", "values": {}})),
        &[],
    );
    assert!(both.is_err());
    let neither = execute("text.render", &args(json!({"values": {}})), &[]);
    assert!(neither.is_err());
    assert!(render("{{ a }}", json!(["not", "an", "object"])).is_err());
}

#[test]
fn template_path_is_read_relative_to_the_program_directory() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("alacritty.toml.tpl"),
        ALACRITTY_TEMPLATE,
    )
    .unwrap();
    let value = execute_in_cwd(
        "text.render",
        &args(json!({"template_path": "alacritty.toml.tpl", "values": theme()})),
        &[],
        directory.path(),
    )
    .unwrap();
    assert_eq!(value, render(ALACRITTY_TEMPLATE, theme()).unwrap());

    let missing = execute_in_cwd(
        "text.render",
        &args(json!({"template_path": "absent.tpl", "values": {}})),
        &[],
        directory.path(),
    )
    .unwrap_err();
    assert_eq!(missing.exit_status(), 1);
}

#[test]
fn template_must_be_utf8_and_within_the_size_limit() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("bad.tpl"), [0xffu8, 0xfe]).unwrap();
    let error = execute_in_cwd(
        "text.render",
        &args(json!({"template_path": "bad.tpl", "values": {}})),
        &[],
        directory.path(),
    )
    .unwrap_err();
    assert_eq!(error.exit_status(), 2);
    // A value that expands past the output cap fails instead of allocating more.
    let huge = "v".repeat(1024 * 1024);
    let template = "{{ v }}".repeat(17);
    assert!(render(&template, json!({"v": huge})).is_err());
}
