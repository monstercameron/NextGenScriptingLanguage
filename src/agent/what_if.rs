//! `execute run FILE --what-if` and `--confirm FINGERPRINT`: confirm the plan you
//! reviewed. `--what-if` prints the plan with its fingerprint and runs nothing.
//! `--confirm` runs only when the current fingerprint equals the one given.
//!
//! The fingerprint covers everything that decides what the run does: the explain
//! plan; the resolved input names, types, and values; the working directory; the
//! capability and data-flow policy file contents; the manifest files; and the
//! program's version. A sensitive input (an environment value or a secret prompt
//! answer, see `ast::is_sensitive`) is a candidate secret. The review shows it as an
//! HMAC-SHA256 under a per-user key (see `what_if_key`), so a changed secret still
//! changes the fingerprint, and an offline guess needs the key.

use crate::agent::what_if_key::{self, hmac_sha256};
use crate::ast::{InputDeclaration, InputSource, Value as ProgramValue};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Protocol identifier of the `--what-if` object.
pub const PROTOCOL: &str = "execute.what_if/2";

/// What a review covers besides the explain plan: the settings the run would use.
pub struct RunSettings<'a> {
    pub declarations: &'a [InputDeclaration],
    /// The supplied input values, as the run would receive them.
    pub inputs: &'a BTreeMap<String, ProgramValue>,
    pub cwd: &'a Path,
    pub capability_policy: Option<&'a Path>,
    pub data_flow_policy: Option<&'a Path>,
    pub manifest_dir: Option<&'a Path>,
    pub manifest_catalog: Option<&'a Path>,
    pub max_loop_iterations: usize,
    pub loop_budget_scope: crate::runtime::LoopBudgetScope,
}

/// SHA-256 as lowercase hex.
fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A file's digest, or null when no file is named.
fn file_digest(path: Option<&Path>) -> Result<Value> {
    let Some(path) = path else {
        return Ok(Value::Null);
    };
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "sha256": digest(&bytes),
    }))
}

/// The manifest files a run loads: the `.json` files of a directory, as the loader
/// reads them, or one file or catalog.
fn manifest_digests(context: &RunSettings<'_>) -> Result<Value> {
    let mut files: Vec<Value> = Vec::new();
    let mut module_paths = BTreeSet::new();
    if let Some(dir) = context.manifest_dir {
        if dir.is_dir() {
            let mut paths: Vec<PathBuf> = fs::read_dir(dir)
                .with_context(|| format!("reading manifest directory {}", dir.display()))?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<_>>()?;
            paths.retain(|path| path.extension().is_some_and(|ext| ext == "json"));
            paths.sort();
            for path in paths {
                let bytes = fs::read(&path)
                    .with_context(|| format!("reading manifest {}", path.display()))?;
                collect_module_paths(&path, &bytes, &mut module_paths)?;
                files.push(json!({
                    "file": path.file_name().map(|name| name.to_string_lossy().into_owned()),
                    "sha256": digest(&bytes),
                }));
            }
        } else {
            let bytes =
                fs::read(dir).with_context(|| format!("reading manifest {}", dir.display()))?;
            collect_module_paths(dir, &bytes, &mut module_paths)?;
            files.push(file_digest(Some(dir))?);
        }
    }
    if let Some(catalog) = context.manifest_catalog {
        let bytes = fs::read(catalog)
            .with_context(|| format!("reading manifest catalog {}", catalog.display()))?;
        collect_module_paths(catalog, &bytes, &mut module_paths)?;
        files.push(json!({ "catalog": file_digest(Some(catalog))? }));
    }
    for path in module_paths {
        files.push(file_digest(Some(&path))?);
    }
    Ok(Value::Array(files))
}

fn collect_module_paths(
    manifest_path: &Path,
    bytes: &[u8],
    paths: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let value: Value = serde_json::from_slice(bytes)
        .with_context(|| format!("parsing manifest {}", manifest_path.display()))?;
    let manifests = value
        .get("manifests")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(std::slice::from_ref(&value));
    for manifest in manifests {
        let Some(program) = manifest
            .pointer("/contract/program")
            .and_then(Value::as_object)
        else {
            continue;
        };
        let parent = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        for key in ["source", "package_manifest", "package_lock"] {
            if let Some(path) = program.get(key).and_then(Value::as_str) {
                // A package manifest is inserted by the walk itself, which also hashes
                // its source and dependencies; inserting it here first would stop that
                // walk before it starts.
                if key == "package_manifest" {
                    collect_package_paths(&parent.join(path), paths, &mut BTreeSet::new())?;
                } else {
                    paths.insert(parent.join(path));
                }
            }
        }
    }
    Ok(())
}

/// Add a package manifest, its source and its dependencies, recursively. `visited`
/// only breaks dependency cycles; a manifest already in `paths` for another reason is
/// still walked.
fn collect_package_paths(
    path: &Path,
    paths: &mut BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if !visited.insert(path.to_path_buf()) {
        return Ok(());
    }
    paths.insert(path.to_path_buf());
    let bytes =
        fs::read(path).with_context(|| format!("reading package manifest {}", path.display()))?;
    let package: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing package manifest {}", path.display()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let Some(source) = package.get("source").and_then(Value::as_str) {
        paths.insert(parent.join(source));
    }
    if let Some(dependencies) = package.get("dependencies").and_then(Value::as_object) {
        for dependency in dependencies.values() {
            if let Some(manifest) = dependency.get("manifest").and_then(Value::as_str) {
                collect_package_paths(&parent.join(manifest), paths, visited)?;
            }
        }
    }
    Ok(())
}

/// The resolved input values. A sensitive input (an environment value or a secret
/// prompt answer; see `ast::is_sensitive`) is shown as an HMAC under the per-user
/// key, bound to the input's name. Without the key, the digest cannot be checked
/// against a guess. With a sensitive input present and no usable key, the review
/// fails rather than falling back to an unkeyed value.
fn input_covered(context: &RunSettings<'_>) -> Result<Value> {
    let values = crate::inputs::resolve(context.declarations, context.inputs, false)?;
    let key = if context.declarations.iter().any(crate::ast::is_sensitive) {
        Some(what_if_key::load_or_create().context(
            "an environment input is sensitive, so the what-if review needs its private key",
        )?)
    } else {
        None
    };
    let mut inputs = Vec::new();
    for declaration in context.declarations {
        let value = values
            .get(&declaration.name)
            .context("a declared input has no resolved value")?;
        let literal = crate::inputs::literal_json(value)?;
        let sensitive = crate::ast::is_sensitive(declaration);
        let shown = match (&key, sensitive) {
            (Some(key), true) => {
                let mut message = declaration.name.as_bytes().to_vec();
                message.push(0);
                message.extend(serde_json::to_vec(&literal)?);
                json!({ "sensitive": true, "hmac_sha256": hex(&hmac_sha256(key, &message)) })
            }
            _ => literal,
        };
        let source = match &declaration.source {
            Some(InputSource::Environment(_)) => "environment",
            Some(InputSource::Prompt { secret: true, .. }) => "secret_prompt",
            _ => "other",
        };
        inputs.push(json!({
            "name": declaration.name,
            "type": declaration.kind.to_string(),
            "source": source,
            "value": shown,
        }));
    }
    Ok(Value::Array(inputs))
}

/// The settings a review covers, without the plan. Their digests stand in for file
/// contents, so the report stays small.
fn resolved_executables(plan: &Value, cwd: &Path) -> Result<Value> {
    let mut executables = BTreeMap::<String, Value>::new();
    // A relative executable path, or a PATH lookup through a relative PATH entry,
    // resolves against the folder current at that step. After a folder change the
    // review cannot know that folder, so it refuses to bind such an executable.
    let relative_path_entry = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|entry| !entry.is_absolute()));
    let mut folder_changed = false;
    for row in plan["steps"].as_array().into_iter().flatten() {
        if row["cwd_change"] == true {
            folder_changed = true;
        }
        if row["process"] == false {
            continue;
        }
        if row["builtin"] == true {
            continue;
        }
        let Some(binary) = row["binary"].as_str() else {
            continue;
        };
        if row["environment_tainted"] == true && row["environment_path_only"] != true {
            bail!(
                "cannot bind the executable for line {} after a run-time environment change",
                row["line"].as_u64().unwrap_or(0)
            );
        }
        let resolved = if let Some(resolved) = row["resolved_path"].as_str() {
            PathBuf::from(resolved)
        } else {
            if row["environment_tainted"] == true {
                bail!(
                    "cannot bind the executable for line {} after a run-time environment change",
                    row["line"].as_u64().unwrap_or(0)
                );
            }
            let has_separator = binary.contains('/') || binary.contains('\\');
            if folder_changed
                && !Path::new(binary).is_absolute()
                && (has_separator || relative_path_entry)
            {
                bail!(
                    "cannot bind the executable {binary:?} for line {} after a folder change: it resolves relative to a folder the review cannot know",
                    row["line"].as_u64().unwrap_or(0)
                );
            }
            // Without the explain plan's manifest-anchored `resolved_path`,
            // this review cannot reproduce the manifest-directory anchor a
            // launch uses for separator-bearing or absolute applet binaries;
            // resolving them against the review cwd instead would bind (and
            // fingerprint) a different file than execution runs. Refuse that
            // rather than reviewing the wrong executable. Bare names keep the
            // hardened PATH lookup, which matches launch.
            if Path::new(binary).is_absolute()
                || binary.contains('/')
                || (cfg!(windows) && binary.contains('\\'))
            {
                bail!(
                    "cannot bind the executable {binary:?} for line {} without its manifest-anchored resolved path",
                    row["line"].as_u64().unwrap_or(0)
                );
            }
            crate::runtime::binary_path(binary, cwd)
                .with_context(|| format!("resolving executable {binary:?} for review"))?
        };
        let canonical = fs::canonicalize(&resolved)
            .with_context(|| format!("canonicalizing executable {}", resolved.display()))?;
        let effective_path_hmac_sha256 = row["effective_path_hmac_sha256"]
            .as_str()
            .context("what-if plan is missing its effective PATH identity")?;
        let mut file = fs::File::open(&canonical)
            .with_context(|| format!("reading executable {}", canonical.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .with_context(|| format!("hashing executable {}", canonical.display()))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        executables.insert(
            format!("{binary}@line:{}", row["line"].as_u64().unwrap_or(0)),
            json!({
                "path": canonical.to_string_lossy(),
                "sha256": hex(&hasher.finalize()),
                "effective_path_hmac_sha256": effective_path_hmac_sha256,
            }),
        );
    }
    Ok(Value::Object(executables.into_iter().collect()))
}

fn settings_covered(plan: &Value, context: &RunSettings<'_>) -> Result<Value> {
    let cwd = fs::canonicalize(context.cwd)
        .with_context(|| format!("resolving {}", context.cwd.display()))?;
    Ok(json!({
        "inputs": input_covered(context)?,
        "cwd": cwd.to_string_lossy(),
        "executables": resolved_executables(plan, context.cwd)?,
        "capability_policy": file_digest(context.capability_policy)?,
        "data_flow_policy": file_digest(context.data_flow_policy)?,
        "manifests": manifest_digests(context)?,
        "max_loop_iterations": context.max_loop_iterations,
        "loop_budget_scope": match context.loop_budget_scope {
            crate::runtime::LoopBudgetScope::PerLoop => "per_loop",
            crate::runtime::LoopBudgetScope::LegacySharedRun => "legacy_shared_run",
        },
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// The fingerprint of a plan and the settings it runs under, and the settings that
/// the fingerprint covers.
pub fn fingerprint(plan: &Value, context: &RunSettings<'_>) -> Result<(String, Value)> {
    let settings = settings_covered(plan, context)?;
    let covered = json!({ "plan": strip_path_review_tokens(plan.clone()), "settings": settings });
    Ok((digest(&serde_json::to_vec(&covered)?), settings))
}

fn strip_path_review_tokens(mut plan: Value) -> Value {
    if let Some(rows) = plan["steps"].as_array_mut() {
        for row in rows {
            if let Some(object) = row.as_object_mut() {
                object.remove("effective_path_hmac_sha256");
            }
        }
    }
    plan
}

/// The `--what-if` report: the fingerprint to confirm, the plan it covers, and the
/// settings it covers.
pub fn report(program_path: &str, fingerprint: &str, plan: Value, settings: Value) -> Value {
    let plan = strip_path_review_tokens(plan);
    json!({
        "protocol": if plan["protocol"] == crate::agent::explain::LEGACY_PROTOCOL { "execute.what_if/1" } else { PROTOCOL },
        "program": program_path,
        "fingerprint": fingerprint,
        "plan": plan,
        "settings": settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_source_changes_the_review_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let manifests = directory.path().join("manifests");
        fs::create_dir(&manifests).unwrap();
        let module_manifest =
            json!({"contract":{"backend":"program_module","program":{"source":"body.program"}}});
        fs::write(manifests.join("module.json"), module_manifest.to_string()).unwrap();
        let source = manifests.join("body.program");
        fs::write(&source, "reviewed body").unwrap();
        let declarations = [];
        let inputs = BTreeMap::new();
        let context = RunSettings {
            declarations: &declarations,
            inputs: &inputs,
            cwd: directory.path(),
            capability_policy: None,
            data_flow_policy: None,
            manifest_dir: Some(&manifests),
            manifest_catalog: None,
            max_loop_iterations: crate::runtime::DEFAULT_MAX_LOOP_ITERATIONS,
            loop_budget_scope: crate::runtime::LoopBudgetScope::PerLoop,
        };
        let plan = json!({"steps": []});
        let before = fingerprint(&plan, &context).unwrap().0;
        fs::write(&source, "changed body").unwrap();
        let after = fingerprint(&plan, &context).unwrap().0;
        assert_ne!(before, after);
    }

    #[test]
    fn package_source_and_dependency_changes_change_the_review_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let manifests = directory.path().join("manifests");
        let dependency = directory.path().join("dep");
        fs::create_dir(&manifests).unwrap();
        fs::create_dir(&dependency).unwrap();
        let module_manifest = json!({"contract":{"backend":"program_module","program":{"source":"body.program","package_manifest":"package.json"}}});
        fs::write(manifests.join("module.json"), module_manifest.to_string()).unwrap();
        fs::write(manifests.join("body.program"), "body").unwrap();
        fs::write(
            manifests.join("package.json"),
            json!({"source":"package.program","dependencies":{"dep":{"manifest":"../dep/package.json"}}}).to_string(),
        )
        .unwrap();
        fs::write(manifests.join("package.program"), "package source").unwrap();
        fs::write(
            dependency.join("package.json"),
            json!({"source":"dep.program"}).to_string(),
        )
        .unwrap();
        fs::write(dependency.join("dep.program"), "dependency source").unwrap();
        let declarations = [];
        let inputs = BTreeMap::new();
        let context = RunSettings {
            declarations: &declarations,
            inputs: &inputs,
            cwd: directory.path(),
            capability_policy: None,
            data_flow_policy: None,
            manifest_dir: Some(&manifests),
            manifest_catalog: None,
            max_loop_iterations: crate::runtime::DEFAULT_MAX_LOOP_ITERATIONS,
            loop_budget_scope: crate::runtime::LoopBudgetScope::PerLoop,
        };
        let plan = json!({"steps": []});
        let first = fingerprint(&plan, &context).unwrap().0;
        fs::write(manifests.join("package.program"), "changed package source").unwrap();
        let second = fingerprint(&plan, &context).unwrap().0;
        assert_ne!(first, second, "package source");
        fs::write(dependency.join("dep.program"), "changed dependency source").unwrap();
        let third = fingerprint(&plan, &context).unwrap().0;
        assert_ne!(second, third, "dependency source");
    }

    #[test]
    fn resolved_executable_path_and_digest_are_part_of_review_settings() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory
            .path()
            .join(if cfg!(windows) { "first.exe" } else { "first" });
        let second = directory.path().join(if cfg!(windows) {
            "second.exe"
        } else {
            "second"
        });
        fs::write(&first, b"first executable").unwrap();
        fs::write(&second, b"second executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&first, &second] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let first_settings = resolved_executables(
            &json!({"steps":[{"binary":first.to_string_lossy(),"direct":true,"effective_path_hmac_sha256":"a"}]}),
            directory.path(),
        )
        .unwrap();
        let second_settings = resolved_executables(
            &json!({"steps":[{"binary":second.to_string_lossy(),"direct":true,"effective_path_hmac_sha256":"a"}]}),
            directory.path(),
        )
        .unwrap();
        assert_ne!(first_settings, second_settings);
        assert!(first_settings.to_string().contains("sha256"));
    }

    #[test]
    fn review_refuses_executable_resolution_after_environment_taint() {
        let directory = tempfile::tempdir().unwrap();
        let error = resolved_executables(
            &json!({"steps":[{"binary":"tool","process":true,"environment_tainted":true,"line":2}]}),
            directory.path(),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("after a run-time environment change"));
    }

    #[test]
    fn review_refuses_a_resolved_executable_after_environment_taint() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory
            .path()
            .join(if cfg!(windows) { "tool.exe" } else { "tool" });
        fs::write(&executable, b"review fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let resolved_path = fs::canonicalize(executable).unwrap();
        let plan = json!({"steps":[{
            "binary":"tool",
            "process":true,
            "environment_tainted":true,
            "environment_path_only":false,
            "resolved_path":resolved_path,
            "line":2
        }]});
        let error = resolved_executables(&plan, directory.path()).unwrap_err();
        assert!(error
            .to_string()
            .contains("after a run-time environment change"));
    }
}
