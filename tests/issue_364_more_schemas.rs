//! Issue #364, second slice: the published schemas for `execute.doctor/1`,
//! `execute.what_if/2`, `execute.elevated_applet/1` and `execute.migrate-scan/1`
//! accept the output the library builds and reject damage. Each output comes from a
//! public library function: `flow::doctor::run_checks` and `render_json` over a fake
//! `Environment`, `flow::agent::what_if::fingerprint` and `report` over a plan from
//! `flow::agent::explain::explain_json`, `flow::privilege::elevated::encode_request`,
//! and `flow::migrate_scan::scan` and `render_json` over a temporary home directory.
//! The checker is the one `tests/issue_364_protocol_schemas.rs` uses.
//! The what-if test needs `agent-advisory`, because its module is gated on it; the
//! other three need no feature.

#![cfg(feature = "agent-advisory")]

use flow::doctor::{DirState, Environment, FileKind, Platform};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const DOCTOR: &str = "docs/schemas/doctor.v1.schema.json";
const WHAT_IF: &str = "docs/schemas/what-if.v2.schema.json";
const ELEVATED: &str = "docs/schemas/elevated-applet.v1.schema.json";
const MIGRATE: &str = "docs/schemas/migrate-scan.v1.schema.json";

fn project() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn load_schema(relative: &str) -> Value {
    let text = std::fs::read_to_string(project().join(relative))
        .unwrap_or_else(|error| panic!("reading {relative}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parsing {relative}: {error}"))
}

fn matches_type(instance: &Value, kind: &str) -> bool {
    match kind {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "integer" => instance.is_i64() || instance.is_u64(),
        "number" => instance.is_number(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => false,
    }
}

fn violations(schema: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| {
                let mut branch_errors = Vec::new();
                violations(branch, instance, path, &mut branch_errors);
                branch_errors.is_empty()
            })
            .count();
        if matching != 1 {
            errors.push(format!(
                "{path}: matches {matching} oneOf branches, expected exactly 1"
            ));
        }
    }
    if let Some(constant) = schema.get("const") {
        if constant != instance {
            errors.push(format!("{path}: expected {constant}"));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(instance) {
            errors.push(format!("{path}: {instance} is not an allowed value"));
        }
    }
    if let Some(kind) = schema.get("type") {
        let kinds: Vec<&str> = match kind {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !kinds.iter().any(|name| matches_type(instance, name)) {
            errors.push(format!("{path}: expected type {kind}"));
            return;
        }
    }
    if let (Some(minimum), Some(number)) = (
        schema.get("minimum").and_then(Value::as_f64),
        instance.as_f64(),
    ) {
        if number < minimum {
            errors.push(format!("{path}: {number} is below {minimum}"));
        }
    }
    match instance {
        Value::Object(map) => {
            for name in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = name.as_str() {
                    if !map.contains_key(name) {
                        errors.push(format!("{path}: missing required property {name}"));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
            for (key, value) in map {
                match properties.and_then(|known| known.get(key)) {
                    Some(sub) => violations(sub, value, &format!("{path}/{key}"), errors),
                    None if closed => errors.push(format!("{path}: unexpected property {key}")),
                    None => {
                        if let Some(sub) =
                            schema.get("additionalProperties").filter(|s| s.is_object())
                        {
                            violations(sub, value, &format!("{path}/{key}"), errors);
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    violations(item_schema, item, &format!("{path}/{index}"), errors);
                }
            }
        }
        _ => {}
    }
}

fn assert_conforms(schema_file: &str, instance: &Value) {
    let schema = load_schema(schema_file);
    let mut errors = Vec::new();
    violations(&schema, instance, "", &mut errors);
    assert!(
        errors.is_empty(),
        "{schema_file} rejected real output: {errors:#?}\n{instance:#}"
    );
}

fn assert_rejects(schema_file: &str, damaged: &Value) {
    let schema = load_schema(schema_file);
    let mut errors = Vec::new();
    violations(&schema, damaged, "", &mut errors);
    assert!(
        !errors.is_empty(),
        "{schema_file} accepted a damaged document: {damaged:#}"
    );
}

/// A machine for `flow::doctor::run_checks`: a Linux install under `/opt/execute`, with
/// a desktop session when `complete` is true and a broken one otherwise.
struct Machine {
    vars: BTreeMap<String, String>,
    files: BTreeSet<PathBuf>,
    dirs: BTreeSet<PathBuf>,
}

impl Machine {
    fn install(complete: bool) -> Self {
        let bin = PathBuf::from("/opt/execute/bin");
        let mut machine = Machine {
            vars: BTreeMap::new(),
            files: BTreeSet::new(),
            dirs: BTreeSet::from([
                PathBuf::from("/opt/execute/share/execute/manifests"),
                PathBuf::from("/run/user/1000"),
            ]),
        };
        let vars: &[(&str, &str)] = if complete {
            &[
                ("HOME", "/home/u"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("HYPRLAND_INSTANCE_SIGNATURE", "abc"),
                ("WAYLAND_DISPLAY", "wayland-1"),
            ]
        } else {
            &[("HYPRLAND_INSTANCE_SIGNATURE", "abc")]
        };
        for (key, value) in vars {
            machine.vars.insert((*key).to_owned(), (*value).to_owned());
        }
        machine.files.insert(bin.join("execute-applet"));
        if complete {
            machine.files.insert(bin.join("execute-model"));
        }
        machine
    }
}

impl Environment for Machine {
    fn platform(&self) -> Platform {
        Platform::Linux
    }

    fn build_version(&self) -> &str {
        "0.9.0"
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    fn current_exe(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/opt/execute/bin/execute"))
    }

    fn file_kind(&self, path: &Path) -> Result<FileKind, String> {
        if self.dirs.contains(path) {
            Ok(FileKind::Directory)
        } else if self.files.contains(path) {
            Ok(FileKind::File)
        } else {
            Ok(FileKind::Missing)
        }
    }

    fn dir_state(&self, _path: &Path) -> DirState {
        DirState::Writable
    }

    fn list_names(&self, _dir: &Path) -> Result<Vec<String>, String> {
        Ok(vec!["awk.json".to_owned()])
    }

    fn helper_version(&self, helper: &Path) -> Result<String, String> {
        let name = helper
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(format!("{name} 0.9.0"))
    }

    fn find_on_path(&self, _binary: &str) -> Option<PathBuf> {
        None
    }

    fn bundled_manifest_files(&self) -> Result<Vec<String>, String> {
        Ok(vec!["awk.json".to_owned()])
    }

    fn bundled_manifest_digests(&self) -> Result<Vec<(String, String)>, String> {
        Ok(vec![("awk.json".to_owned(), "0".repeat(64))])
    }

    fn file_digest(&self, _path: &Path) -> Result<String, String> {
        Ok("0".repeat(64))
    }

    fn helper_trust(&self, _helper: &Path, _name: &str) -> Result<(), String> {
        Ok(())
    }

    fn current_uid(&self) -> Option<u32> {
        Some(1000)
    }

    fn unix_owner_and_mode(&self, _path: &Path) -> Result<(u32, u32), String> {
        Ok((1000, 0o700))
    }
}

#[test]
fn doctor_schema_accepts_check_reports_and_rejects_damage() {
    let healthy =
        flow::doctor::render_json(&flow::doctor::run_checks(&Machine::install(true)), "0.9.0");
    assert_conforms(DOCTOR, &healthy);
    assert_eq!(healthy["protocol"], "execute.doctor/1");
    let problems: Vec<Value> = healthy["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["status"] == "fail")
        .cloned()
        .collect();
    let names: Vec<&str> = problems
        .iter()
        .map(|check| check["id"].as_str().unwrap_or("?"))
        .collect();
    assert!(names.is_empty(), "failing ids: {}", names.join(" "));

    let broken =
        flow::doctor::render_json(&flow::doctor::run_checks(&Machine::install(false)), "0.9.0");
    assert_conforms(DOCTOR, &broken);
    assert_eq!(broken["ok"], false);
    let statuses: BTreeSet<&str> = broken["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["status"].as_str().unwrap())
        .collect();
    assert!(
        statuses.contains("fail") && statuses.contains("skipped"),
        "{statuses:?}"
    );

    let mut missing = broken.clone();
    missing.as_object_mut().unwrap().remove("counts");
    assert_rejects(DOCTOR, &missing);
    let mut extra = broken.clone();
    extra["surprise"] = json!(true);
    assert_rejects(DOCTOR, &extra);
    let mut status = broken.clone();
    status["checks"][0]["status"] = json!("maybe");
    assert_rejects(DOCTOR, &status);
    let mut id = broken.clone();
    id["checks"][0]["id"] = json!("helpers.other");
    assert_rejects(DOCTOR, &id);
    let mut remedy = healthy;
    remedy["checks"][0]["remedy"] = json!(7);
    assert_rejects(DOCTOR, &remedy);
}

#[cfg(feature = "agent-advisory")]
#[test]
fn what_if_schema_accepts_report_and_rejects_damage() {
    let program =
        flow::parser::parse("accept name as text with default \"x\"\nuse value name\n").unwrap();
    let registry = flow::manifest::Registry::default();
    let plan = flow::agent::explain::explain_json(&program, &registry, "what-if.program").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let inputs = BTreeMap::from([(
        "name".to_owned(),
        flow::ast::Value::String("alice".to_owned()),
    )]);
    let context = flow::agent::what_if::RunSettings {
        declarations: &program.inputs,
        inputs: &inputs,
        cwd: directory.path(),
        capability_policy: None,
        data_flow_policy: None,
        manifest_dir: None,
        manifest_catalog: None,
        max_loop_iterations: flow::runtime::DEFAULT_MAX_LOOP_ITERATIONS,
        loop_budget_scope: flow::runtime::LoopBudgetScope::PerLoop,
    };
    let (fingerprint, settings) = flow::agent::what_if::fingerprint(&plan, &context).unwrap();
    let report = flow::agent::what_if::report("what-if.program", &fingerprint, plan, settings);
    assert_conforms(WHAT_IF, &report);
    assert_eq!(report["protocol"], "execute.what_if/2");
    assert_eq!(report["settings"]["inputs"][0]["name"], "name");

    let mut legacy = report.clone();
    legacy["protocol"] = json!("execute.what_if/1");
    assert_rejects(WHAT_IF, &legacy);
    let mut without_fingerprint = report.clone();
    without_fingerprint
        .as_object_mut()
        .unwrap()
        .remove("fingerprint");
    assert_rejects(WHAT_IF, &without_fingerprint);
    let mut extra_setting = report.clone();
    extra_setting["settings"]["surprise"] = json!(true);
    assert_rejects(WHAT_IF, &extra_setting);
    let mut budget = report.clone();
    budget["settings"]["loop_budget_scope"] = json!("sometimes");
    assert_rejects(WHAT_IF, &budget);
    let mut source = report;
    source["settings"]["inputs"][0]["source"] = json!("stdin");
    assert_rejects(WHAT_IF, &source);
}

#[test]
fn elevated_applet_schema_accepts_encoded_request_and_rejects_damage() {
    let arguments: Map<String, Value> = Map::from_iter([("path".to_owned(), json!("/etc/hosts"))]);
    let request = flow::privilege::elevated::ElevatedRequest::new("copy_file", arguments).unwrap();
    let bytes = flow::privilege::elevated::encode_request(&request).unwrap();
    let object: Value = serde_json::from_slice(&bytes).unwrap();
    assert_conforms(ELEVATED, &object);
    assert_eq!(object["protocol"], "execute.elevated_applet/1");
    assert_eq!(object["applet"], "copy_file");

    let mut wrong_protocol = object.clone();
    wrong_protocol["protocol"] = json!("execute.elevated_applet/2");
    assert_rejects(ELEVATED, &wrong_protocol);
    let mut wrong_version = object.clone();
    wrong_version["version"] = json!(2);
    assert_rejects(ELEVATED, &wrong_version);
    let mut without_arguments = object.clone();
    without_arguments
        .as_object_mut()
        .unwrap()
        .remove("arguments");
    assert_rejects(ELEVATED, &without_arguments);
    let mut extra = object.clone();
    extra["surprise"] = json!(true);
    assert_rejects(ELEVATED, &extra);
    let mut applet = object;
    applet["applet"] = json!(7);
    assert_rejects(ELEVATED, &applet);
}

#[test]
fn migrate_scan_schema_accepts_scan_report_and_rejects_damage() {
    let home = tempfile::tempdir().unwrap();
    let local_bin = home.path().join(".local").join("bin");
    fs::create_dir_all(&local_bin).unwrap();
    fs::write(
        local_bin.join("tidy"),
        "#!/bin/sh\ntrap 'rm -f /tmp/tidy.lock' EXIT\necho tidy\n",
    )
    .unwrap();
    let hooks = home.path().join(".config").join("omarchy").join("hooks");
    fs::create_dir_all(hooks.join("theme-set.d")).unwrap();
    fs::write(hooks.join("theme-set"), "#!/bin/sh\necho set\n").unwrap();
    fs::write(
        hooks.join("theme-set.d").join("notes.txt"),
        "not a script\n",
    )
    .unwrap();

    let report = flow::migrate_scan::scan(home.path()).unwrap();
    let value = flow::migrate_scan::render_json(&report);
    assert_conforms(MIGRATE, &value);
    assert_eq!(value["protocol"], "execute.migrate-scan/1");
    assert_eq!(value["totals"]["entries"], 3);
    let kinds: BTreeSet<&str> = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, BTreeSet::from(["hook", "hook_d", "local_bin"]));

    let mut verdict = value.clone();
    verdict["entries"][0]["verdict"] = json!("maybe");
    assert_rejects(MIGRATE, &verdict);
    let mut kind = value.clone();
    kind["entries"][0]["kind"] = json!("hook_x");
    assert_rejects(MIGRATE, &kind);
    let mut without_reason = value.clone();
    without_reason["entries"][0]
        .as_object_mut()
        .unwrap()
        .remove("reason");
    assert_rejects(MIGRATE, &without_reason);
    let mut finding = value.clone();
    finding["findings"] = json!([{"kind": "guess", "path": "x", "detail": "y"}]);
    assert_rejects(MIGRATE, &finding);
    let mut totals = value.clone();
    totals["totals"]["surprise"] = json!(1);
    assert_rejects(MIGRATE, &totals);
    let mut category = value;
    category["entries"][0]["features"] = json!([
        {"name": "trap", "category": "teleport", "need": "none"}
    ]);
    assert_rejects(MIGRATE, &category);
}
