//! Local, integrity-checked packages for finite `.program` applet modules.
//!
//! This module deliberately performs no installation or network access. A
//! package is loaded from an explicit manifest path, all paths are resolved
//! relative to the file that declares them, and optional lock data is verified
//! against the complete local dependency graph.

use crate::ast::{InputType, Invocation, Program, ReusableProgram, Step, StepKind, Trigger};
pub use crate::contracts::ProgramModule;
use crate::contracts::{AppletBackend, AppletContract, Lifecycle};
use crate::contracts::{OutcomeKind, OutcomeSpec, OutputSchema, OutputType};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const PROGRAM_MODULE_INTERFACE_VERSION: u32 = 1;
pub const PROGRAM_MODULE_PACKAGE_FORMAT: u32 = 1;
pub const PROGRAM_MODULE_LOCK_FORMAT: u32 = 1;
pub const MAX_PROGRAM_MODULE_DEPTH: usize = 16;
pub const MAX_PROGRAM_MODULE_PACKAGES: usize = 128;
pub const MAX_PROGRAM_MODULES_PER_RUN: usize = 128;
pub const MAX_PROGRAM_MODULE_GRAPH_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PROGRAM_MODULE_RUN_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODULE_MANIFEST_BYTES: usize = 1024 * 1024;

/// Strict JSON package manifest format. `source_sha256` pins the `.program`
/// source bytes. Every dependency pins its manifest bytes, exact local path,
/// package source, package version requirement, and interface-version range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramModulePackageManifest {
    pub format_version: u32,
    pub name: String,
    pub version: String,
    pub interface_version: u32,
    pub source: PathBuf,
    pub source_sha256: String,
    pub exports: BTreeMap<String, ProgramModuleExport>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, ProgramModuleDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramModuleExport {
    pub entry: String,
    pub interface_version: u32,
    pub inputs: Vec<ProgramModuleInput>,
    pub output: OutputSchema,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<OutcomeSpec>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub result_envelope: bool,
    pub lifecycle: ModuleLifecycle,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramModuleInputType {
    Text,
    Integer,
    Boolean,
    TextList,
    Json,
}

impl From<ProgramModuleInputType> for InputType {
    fn from(value: ProgramModuleInputType) -> Self {
        match value {
            ProgramModuleInputType::Text => InputType::Text,
            ProgramModuleInputType::Integer => InputType::Integer,
            ProgramModuleInputType::Boolean => InputType::Boolean,
            ProgramModuleInputType::TextList => InputType::TextList,
            ProgramModuleInputType::Json => InputType::Json,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramModuleInput {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ProgramModuleInputType,
    pub required: bool,
}

/// Lifecycle vocabulary is intentionally broader than the finite runtime so
/// unsupported package declarations produce a useful validation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleLifecycle {
    OneShot,
    Listener,
    Schedule,
    PersistentWorker,
    UiComponent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramModuleDependency {
    /// Relative path to the dependency's package manifest.
    pub manifest: PathBuf,
    /// SHA-256 of the dependency `.program` source bytes.
    pub source_sha256: String,
    /// SHA-256 of the dependency manifest bytes, pinning its export metadata.
    pub manifest_sha256: String,
    /// SemVer range. The supported forms are an exact version or a caret range.
    pub version: String,
    pub interface_min: u32,
    /// Exclusive upper bound on the stable module interface version.
    pub interface_max_exclusive: u32,
}

/// A generated, deterministic lock snapshot. Loading verifies that it exactly
/// describes the reachable package graph; it never rewrites the lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramModuleLock {
    pub lock_version: u32,
    pub root: String,
    pub packages: BTreeMap<String, LockedProgramModule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedProgramModule {
    /// Canonical package-manifest path relative to the module root.
    pub manifest: PathBuf,
    pub version: String,
    pub interface_version: u32,
    pub manifest_sha256: String,
    pub source_sha256: String,
}

#[derive(Debug, Clone)]
pub struct LoadedProgramModule {
    pub reference: ProgramModule,
    pub package_name: String,
    pub package_version: String,
    pub manifest_path: PathBuf,
    pub source_path: PathBuf,
    pub source_sha256: String,
    pub manifest_sha256: String,
    pub program: Arc<Program>,
    pub definition: ReusableProgram,
    pub export: ProgramModuleExport,
    /// Bytes in this package and its verified transitive package graph.
    pub expanded_bytes: usize,
    /// Stable, sorted fingerprints for the root package and every dependency.
    pub transitive_fingerprints: Vec<ModuleFingerprint>,
}

#[derive(Debug, Clone)]
pub struct LoadedProgramSource {
    pub reference: ProgramModule,
    pub source_path: PathBuf,
    pub source_sha256: String,
    pub program: Arc<Program>,
    pub definition: ReusableProgram,
    /// The package export declaration, when this source came from a package.
    pub export: Option<ProgramModuleExport>,
    /// The source and every package manifest/source in the resolved graph.
    pub transitive_fingerprints: Vec<ModuleFingerprint>,
    /// Bytes read for this source and its complete verified package graph.
    pub expanded_bytes: usize,
    /// The verified package graph, when this export was declared by a package.
    /// Retaining it lets preflight build a package-local alias scope without
    /// re-reading paths once execution has started.
    pub package: Option<Arc<LoadedProgramModulePackage>>,
    /// Stable digest for checkpoint identity, including the selected entry.
    pub checkpoint_fingerprint: String,
}

impl LoadedProgramSource {
    /// Ensure the applet manifest publishes the same stable interface as its
    /// selected package export. Direct modules rely on the outer applet
    /// manifest as their interface declaration.
    pub fn validate_contract(&self, contract: &AppletContract) -> Result<()> {
        contract.validate_interface()?;
        if contract.backend != AppletBackend::ProgramModule
            || contract.program.as_ref() != Some(&self.reference)
        {
            bail!("program module descriptor does not match the applet contract");
        }
        if let Some(export) = &self.export {
            if contract.lifecycle != Lifecycle::OneShot
                || contract.output.as_ref() != Some(&export.output)
                || contract.outcomes != export.outcomes
                || contract.result_envelope != export.result_envelope
                || contract.capabilities != export.capabilities
                || contract.effects != export.effects
            {
                bail!("applet contract does not match package export interface metadata");
            }
        }
        Ok(())
    }

    /// Validate the full applet boundary before the caller starts effects.
    /// The manifest owns public parameter types; the selected definition owns
    /// the module's internal input declarations.
    pub fn validate_applet_manifest(&self, manifest: &crate::manifest::Manifest) -> Result<()> {
        let contract = manifest.execution_contract().ok_or_else(|| {
            anyhow!("program module applet requires a versioned execution contract")
        })?;
        self.validate_contract(contract)?;
        let inputs = self
            .definition
            .inputs
            .iter()
            .map(|input| (input.name.clone(), input.kind, input.default.is_none()))
            .collect::<Vec<_>>();
        manifest.validate_program_module_inputs(&inputs)
    }
}

/// Immutable per-execution snapshot of every module directly reachable from
/// a program or graph invocation list. Sources and package metadata are loaded
/// once during preflight and retained for execution, so the runtime never
/// resolves module paths after caller effects have started.
#[derive(Clone, Default)]
pub struct ProgramModuleSession {
    modules: BTreeMap<ModuleKey, Arc<LoadedProgramSource>>,
    child_registries: BTreeMap<ModuleKey, crate::manifest::Registry>,
    fingerprints: BTreeMap<String, String>,
    body_invocations: Vec<(String, Invocation)>,
    scoped_body_invocations: Vec<(crate::manifest::Registry, Invocation)>,
    unpinned_body_calls: Vec<String>,
    expanded_bytes: usize,
}

impl std::fmt::Debug for ProgramModuleSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramModuleSession")
            .field("module_count", &self.modules.len())
            .field("fingerprints", &self.fingerprints)
            .field("body_call_count", &self.body_invocations.len())
            .field("unpinned_body_calls", &self.unpinned_body_calls)
            .field("expanded_bytes", &self.expanded_bytes)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ModuleKey {
    scope: Option<String>,
    origin: PathBuf,
    name: String,
}

impl ModuleKey {
    fn new(
        registry: &crate::manifest::Registry,
        manifest: &crate::manifest::Manifest,
    ) -> Result<Self> {
        Ok(Self {
            scope: registry.scope_identity().map(str::to_owned),
            origin: manifest
                .origin()
                .ok_or_else(|| anyhow!("program module manifest has no file origin"))?
                .to_path_buf(),
            name: manifest.name.clone(),
        })
    }

    fn fingerprint_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.scope.as_deref().unwrap_or("catalog"),
            self.origin.display(),
            self.name
        )
    }
}

impl ProgramModuleSession {
    /// Preload modules referenced by a parsed finite program. All reusable
    /// definitions are scanned as well as top-level statements, matching the
    /// runtime's whole-source preflight behavior.
    pub fn preload_program(
        program: &Program,
        registry: &crate::manifest::Registry,
    ) -> Result<Self> {
        let calls = program_invocations(program);
        Self::preload_invocations(registry, calls)
    }

    /// Preload modules referenced by an already extracted invocation list,
    /// such as a graph plan. The iterator is resolved through the same
    /// registry used for execution; there is no package discovery or fetch.
    pub fn preload_invocations<'a>(
        registry: &crate::manifest::Registry,
        calls: impl IntoIterator<Item = &'a Invocation>,
    ) -> Result<Self> {
        let mut session = Self::default();
        let mut ancestry = Vec::new();
        for call in calls {
            let manifest = registry
                .resolve_call(call)
                .with_context(|| format!("preflighting module call {:?}", call.operation))?;
            if manifest.execution_backend() == AppletBackend::ProgramModule {
                session.preload_manifest(registry, manifest, 0, &mut ancestry)?;
            }
        }
        Ok(session)
    }

    fn preload_manifest(
        &mut self,
        registry: &crate::manifest::Registry,
        manifest: &crate::manifest::Manifest,
        depth: usize,
        ancestry: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let key = ModuleKey::new(registry, manifest)?;
        if self.modules.contains_key(&key) {
            return Ok(());
        }
        let contract = manifest.execution_contract().ok_or_else(|| {
            anyhow!(
                "program module applet {:?} requires an execution contract",
                manifest.name
            )
        })?;
        let reference = contract.program.as_ref().ok_or_else(|| {
            anyhow!(
                "program module applet {:?} has no program descriptor",
                manifest.name
            )
        })?;
        let origin = manifest.origin().ok_or_else(|| {
            anyhow!(
                "program module applet {:?} has no file manifest origin",
                manifest.name
            )
        })?;
        let root = registry.module_root().ok_or_else(|| {
            anyhow!(
                "program module applet {:?} has no configured module root",
                manifest.name
            )
        })?;
        let loaded = Arc::new(
            load_program_module(reference, origin, root)
                .with_context(|| format!("preloading program module applet {:?}", manifest.name))?,
        );
        self.preload_loaded(registry, manifest, key, loaded, depth, ancestry)
    }

    fn preload_loaded(
        &mut self,
        registry: &crate::manifest::Registry,
        manifest: &crate::manifest::Manifest,
        key: ModuleKey,
        loaded: Arc<LoadedProgramSource>,
        depth: usize,
        ancestry: &mut Vec<PathBuf>,
    ) -> Result<()> {
        if self.modules.contains_key(&key) {
            return Ok(());
        }
        if depth >= MAX_PROGRAM_MODULE_DEPTH {
            bail!("program module import depth exceeds {MAX_PROGRAM_MODULE_DEPTH}");
        }
        if self.modules.len() >= MAX_PROGRAM_MODULES_PER_RUN {
            bail!("program execution reaches more than {MAX_PROGRAM_MODULES_PER_RUN} modules");
        }
        loaded.validate_applet_manifest(manifest)?;

        if let Some(package) = &loaded.package {
            if ancestry.contains(&package.manifest_path) {
                bail!(
                    "program module import cycle reaches {}",
                    package.manifest_path.display()
                );
            }
            ancestry.push(package.manifest_path.clone());
        }

        let (child_registry, imported) = match &loaded.package {
            Some(package) => build_dependency_scope(registry, package)?,
            None => (registry.clone(), BTreeMap::new()),
        };
        self.modules.insert(key.clone(), Arc::clone(&loaded));
        self.child_registries
            .insert(key.clone(), child_registry.clone());
        self.fingerprints
            .insert(key.fingerprint_key(), loaded.checkpoint_fingerprint.clone());
        self.expanded_bytes = self
            .expanded_bytes
            .checked_add(loaded.expanded_bytes)
            .ok_or_else(|| anyhow!("program module source size overflow"))?;
        if self.expanded_bytes > MAX_PROGRAM_MODULE_RUN_BYTES {
            bail!(
                "program module sources exceed {MAX_PROGRAM_MODULE_RUN_BYTES} bytes for one execution"
            );
        }

        let child_program = Program {
            definitions: loaded.program.definitions.clone(),
            inputs: loaded.definition.inputs.clone(),
            steps: loaded.definition.steps.clone(),
            trigger: None,
        };
        crate::runtime::validate(&child_program, &child_registry)
            .with_context(|| format!("validating module applet {:?}", manifest.name))?;

        for call in program_invocations_for_source(&loaded.program, &loaded.definition) {
            if let (Some(package), Some((alias, export))) =
                (&loaded.package, call.operation.rsplit_once('.'))
            {
                if package.manifest.dependencies.contains_key(alias) {
                    let child = package.dependencies.get(alias).ok_or_else(|| {
                        anyhow!("verified module dependency {alias:?} was not retained")
                    })?;
                    if !child.manifest.exports.contains_key(export) {
                        bail!(
                            "program module dependency alias {alias:?} does not export {export:?}"
                        );
                    }
                }
            }
            let child_manifest = child_registry.resolve_call(&call).with_context(|| {
                format!(
                    "preflighting call {:?} in module {:?}",
                    call.operation, manifest.name
                )
            })?;
            self.body_invocations
                .push((manifest.name.clone(), call.clone()));
            self.scoped_body_invocations
                .push((child_registry.clone(), call.clone()));
            if child_manifest.execution_backend() == AppletBackend::ProgramModule {
                if !imported.contains_key(&call.operation) || child_manifest.name != call.operation
                {
                    bail!(
                        "program module {:?} invokes ambient program module {:?}; only declared package dependency alias.export imports are allowed",
                        manifest.name,
                        call.operation
                    );
                }
                let imported_loaded = imported
                    .get(&call.operation)
                    .cloned()
                    .ok_or_else(|| anyhow!("declared program module import was not preloaded"))?;
                let imported_key = ModuleKey::new(&child_registry, child_manifest)?;
                self.preload_loaded(
                    &child_registry,
                    child_manifest,
                    imported_key,
                    imported_loaded,
                    depth + 1,
                    ancestry,
                )?;
            }
        }
        if loaded.package.is_some() {
            ancestry.pop();
        }
        Ok(())
    }

    /// Return the preloaded source for a manifest in its exact catalog scope.
    pub fn get_for_manifest(
        &self,
        registry: &crate::manifest::Registry,
        manifest: &crate::manifest::Manifest,
    ) -> Option<&Arc<LoadedProgramSource>> {
        ModuleKey::new(registry, manifest)
            .ok()
            .and_then(|key| self.modules.get(&key))
    }

    /// Registry to use for this module's child calls. Package aliases are
    /// visible only in this immutable, per-module scope.
    pub fn registry_for(
        &self,
        registry: &crate::manifest::Registry,
        manifest: &crate::manifest::Manifest,
    ) -> Option<&crate::manifest::Registry> {
        ModuleKey::new(registry, manifest)
            .ok()
            .and_then(|key| self.child_registries.get(&key))
    }

    /// Deterministic module source identity to include in checkpoint identity.
    pub fn fingerprints(&self) -> &BTreeMap<String, String> {
        &self.fingerprints
    }

    /// Add runtime identities for ordinary applets called by module bodies.
    /// The callback resolves paths and effective environments using the same
    /// runtime rules as execution. Unpinned or dynamically discovered
    /// backends are remembered so checkpointed callers can reject before
    /// replaying completed effects.
    pub(crate) fn fingerprint_body_calls<F>(&mut self, cwd: &Path, mut fingerprint: F) -> Result<()>
    where
        F: FnMut(&crate::manifest::Registry, &Invocation, &Path) -> Result<Option<String>>,
    {
        // The runtime callback receives the caller's initial cwd. A module
        // body can change its scoped cwd before a child invocation, and a
        // relative executable may then resolve to different bytes. Until the
        // loader tracks cwd per call, do not claim a checkpoint identity for
        // ordinary child calls in such a module.
        let mut body_changes_cwd = self.modules.values().any(|loaded| {
            let has_folder_step = |steps: &[Step]| {
                steps
                    .iter()
                    .any(|step| matches!(&step.kind, StepKind::UseFolder { .. }))
            };
            has_folder_step(&loaded.definition.steps)
                || loaded
                    .program
                    .definitions
                    .values()
                    .any(|definition| has_folder_step(&definition.steps))
        });
        for (registry, invocation) in &self.scoped_body_invocations {
            if registry.resolve_call(invocation)?.builtin == Some(crate::manifest::Builtin::Cd) {
                body_changes_cwd = true;
                break;
            }
        }

        for (index, (registry, invocation)) in self.scoped_body_invocations.iter().enumerate() {
            let manifest = registry.resolve_call(invocation)?;
            if manifest.execution_backend() == AppletBackend::ProgramModule {
                // Imported module sources and their transitive package graphs
                // are already represented by `checkpoint_fingerprint`.
                continue;
            }
            if body_changes_cwd {
                self.unpinned_body_calls.push(manifest.name.clone());
                continue;
            }
            match fingerprint(registry, invocation, cwd)? {
                Some(identity) => {
                    let key = format!(
                        "body-call-v1:{index}:{}:{}",
                        registry.scope_identity().unwrap_or("catalog"),
                        manifest.name
                    );
                    self.fingerprints.insert(key, identity);
                }
                None => self.unpinned_body_calls.push(manifest.name.clone()),
            }
        }
        self.unpinned_body_calls.sort();
        self.unpinned_body_calls.dedup();
        Ok(())
    }

    /// Ensure checkpoint identities cover every ordinary applet called by a
    /// module body. Fresh non-checkpointed runs may still use dynamic backends.
    pub fn validate_checkpoint_identity(&self) -> Result<()> {
        if !self.unpinned_body_calls.is_empty() {
            bail!(
                "checkpointed program module calls have unpinned body applets: {}",
                self.unpinned_body_calls.join(", ")
            );
        }
        Ok(())
    }

    /// Calls inside preloaded module bodies, for runtime capability preflight.
    pub fn body_invocations(&self) -> &[(String, Invocation)] {
        &self.body_invocations
    }

    /// Module-body calls paired with the exact scoped catalog that resolved
    /// them, for capability and backend preflight before caller effects.
    pub fn scoped_body_invocations(&self) -> &[(crate::manifest::Registry, Invocation)] {
        &self.scoped_body_invocations
    }
}

fn build_dependency_scope(
    parent_registry: &crate::manifest::Registry,
    package: &LoadedProgramModulePackage,
) -> Result<(
    crate::manifest::Registry,
    BTreeMap<String, Arc<LoadedProgramSource>>,
)> {
    let export_count = package
        .dependencies
        .values()
        .try_fold(0usize, |count, dependency| {
            count.checked_add(dependency.manifest.exports.len())
        })
        .ok_or_else(|| anyhow!("program module dependency export count overflow"))?;
    if export_count > MAX_PROGRAM_MODULES_PER_RUN {
        bail!(
            "program module scope exports more than {MAX_PROGRAM_MODULES_PER_RUN} dependency applets"
        );
    }
    let mut sources = Vec::new();
    let mut loaded_by_alias = BTreeMap::new();
    for (alias, dependency) in &package.dependencies {
        for export_name in dependency.manifest.exports.keys() {
            let exported = dependency.export(export_name)?;
            let relative_source = dependency_source_from_parent(
                &package.manifest,
                alias,
                &dependency.manifest.source,
            )?;
            let reference = ProgramModule {
                source: relative_source,
                entry: export_name.clone(),
                interface_version: exported.export.interface_version,
                package_manifest: None,
                package_lock: None,
            };
            reference.validate()?;
            let identity = format!("{alias}.{export_name}");
            let manifest_json = generated_export_manifest(&identity, &reference, &exported.export)?;
            if loaded_by_alias.contains_key(&identity) {
                bail!("duplicate module import alias {identity:?}");
            }
            let fingerprint = checkpoint_fingerprint(
                &reference,
                &exported.source_path,
                &exported.source_sha256,
                &exported.transitive_fingerprints,
            )?;
            let loaded = LoadedProgramSource {
                reference,
                source_path: exported.source_path,
                source_sha256: exported.source_sha256,
                program: Arc::clone(&exported.program),
                definition: exported.definition,
                export: Some(exported.export),
                transitive_fingerprints: exported.transitive_fingerprints,
                expanded_bytes: 0,
                package: Some(Arc::clone(dependency)),
                checkpoint_fingerprint: fingerprint,
            };
            sources.push(manifest_json);
            loaded_by_alias.insert(identity, Arc::new(loaded));
        }
    }
    let origin = &package.manifest_path;
    let scoped = parent_registry.with_scoped_manifest_sources(&sources, origin)?;
    for identity in loaded_by_alias.keys() {
        let generated = scoped.resolve(identity)?;
        if generated.name != *identity {
            bail!("module import {identity:?} did not resolve to its canonical alias.export name");
        }
    }
    Ok((scoped, loaded_by_alias))
}

fn dependency_source_from_parent(
    parent: &ProgramModulePackageManifest,
    alias: &str,
    source: &Path,
) -> Result<PathBuf> {
    let dependency = parent
        .dependencies
        .get(alias)
        .ok_or_else(|| anyhow!("missing package dependency {alias:?}"))?;
    let dependency_parent = dependency
        .manifest
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let path = dependency_parent.join(source);
    if path.is_absolute() {
        bail!("dependency source path unexpectedly became absolute");
    }
    Ok(path)
}

fn generated_export_manifest(
    name: &str,
    reference: &ProgramModule,
    export: &ProgramModuleExport,
) -> Result<String> {
    let parameters = export
        .inputs
        .iter()
        .map(|input| {
            let (kind, items) = match input.kind {
                ProgramModuleInputType::Text => ("string", None),
                ProgramModuleInputType::Integer => ("integer", None),
                ProgramModuleInputType::Boolean => ("boolean", None),
                ProgramModuleInputType::TextList => ("list", Some("scalar")),
                ProgramModuleInputType::Json => ("json", None),
            };
            let mut schema = serde_json::Map::new();
            schema.insert("type".into(), kind.into());
            schema.insert("required".into(), input.required.into());
            if let Some(items) = items {
                schema.insert("items".into(), items.into());
            }
            (input.name.clone(), serde_json::Value::Object(schema))
        })
        .collect::<serde_json::Map<_, _>>();
    Ok(serde_json::to_string(&serde_json::json!({
        "name": name,
        "phrases": [],
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": parameters,
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": reference,
            "lifecycle": { "kind": "oneshot" },
            "output": export.output,
            "outcomes": export.outcomes,
            "result_envelope": export.result_envelope,
            "capabilities": export.capabilities,
            "effects": export.effects
        }
    }))?)
}

fn program_invocations(program: &Program) -> Vec<&Invocation> {
    let mut calls = Vec::new();
    collect_step_invocations(&program.steps, &mut calls);
    for definition in program.definitions.values() {
        collect_step_invocations(&definition.steps, &mut calls);
    }
    if let Some(Trigger::Listen {
        source: crate::ast::ListenSource::Applet(call),
        ..
    }) = &program.trigger
    {
        calls.push(call);
    }
    calls
}

fn program_invocations_for_source(
    program: &Program,
    selected: &ReusableProgram,
) -> Vec<Invocation> {
    let mut calls = Vec::new();
    collect_step_invocations_owned(&selected.steps, &mut calls);
    // Include any local helper that the parser retained as a definition. The
    // selected body is expanded for ordinary calls, while scanning retained
    // helpers closes over definitions reachable only through a helper call.
    for definition in program.definitions.values() {
        collect_step_invocations_owned(&definition.steps, &mut calls);
    }
    calls
}

fn collect_step_invocations<'a>(steps: &'a [Step], calls: &mut Vec<&'a Invocation>) {
    for step in steps {
        match &step.kind {
            StepKind::Invoke { call, .. } => calls.push(call),
            StepKind::TaskGroup { tasks, .. } => calls.extend(tasks),
            _ => {}
        }
    }
}

fn collect_step_invocations_owned(steps: &[Step], calls: &mut Vec<Invocation>) {
    for step in steps {
        match &step.kind {
            StepKind::Invoke { call, .. } => calls.push(call.clone()),
            StepKind::TaskGroup { tasks, .. } => calls.extend(tasks.iter().cloned()),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleFingerprint {
    pub name: String,
    pub version: String,
    pub interface_version: u32,
    pub manifest_sha256: String,
    pub source_sha256: String,
}

#[derive(Debug, Clone)]
pub struct LoadedProgramModulePackage {
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
    pub source_path: PathBuf,
    pub source_sha256: String,
    pub manifest: ProgramModulePackageManifest,
    pub program: Arc<Program>,
    pub dependencies: BTreeMap<String, Arc<LoadedProgramModulePackage>>,
    /// Total manifest and source bytes in this resolved package graph.
    pub expanded_bytes: usize,
    fingerprints: Vec<ModuleFingerprint>,
}

impl LoadedProgramModulePackage {
    pub fn export(&self, name: &str) -> Result<LoadedProgramModule> {
        let export = self.manifest.exports.get(name).ok_or_else(|| {
            anyhow!(
                "program module {:?} does not export {name:?}",
                self.manifest.name
            )
        })?;
        if export.interface_version != self.manifest.interface_version {
            bail!(
                "export {name:?} declares interface version {}, but package {:?} declares {}",
                export.interface_version,
                self.manifest.name,
                self.manifest.interface_version
            );
        }
        if export.interface_version != PROGRAM_MODULE_INTERFACE_VERSION {
            bail!(
                "unsupported program module interface version {}; supported version is {}",
                export.interface_version,
                PROGRAM_MODULE_INTERFACE_VERSION
            );
        }
        if export.lifecycle != ModuleLifecycle::OneShot {
            bail!(
                "program module export {name:?} uses unsupported {:?} lifecycle; finite callers require one_shot",
                export.lifecycle
            );
        }
        validate_output_schema(&export.output)?;
        let definition = self
            .program
            .definitions
            .get(&export.entry)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "program module {:?} has no entry {:?}",
                    self.manifest.name,
                    export.entry
                )
            })?;
        validate_export_inputs(name, &export.inputs, &definition)?;
        Ok(LoadedProgramModule {
            reference: ProgramModule {
                source: self.manifest.source.clone(),
                // `ProgramModule.entry` names the package's public export;
                // `export.entry` names its private definition in source.
                entry: name.to_owned(),
                interface_version: export.interface_version,
                package_manifest: None,
                package_lock: None,
            },
            package_name: self.manifest.name.clone(),
            package_version: self.manifest.version.clone(),
            manifest_path: self.manifest_path.clone(),
            source_path: self.source_path.clone(),
            source_sha256: self.source_sha256.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            program: Arc::clone(&self.program),
            definition,
            export: export.clone(),
            expanded_bytes: self.expanded_bytes,
            transitive_fingerprints: self.fingerprints.clone(),
        })
    }

    pub fn lock_snapshot(&self, root: &Path) -> Result<ProgramModuleLock> {
        let root = canonical_directory(root, "module root")?;
        self.manifest_path
            .strip_prefix(&root)
            .context("root package manifest is outside module root")?;
        let mut packages = BTreeMap::new();
        self.collect_lock_entries(&root, &mut packages)?;
        Ok(ProgramModuleLock {
            lock_version: PROGRAM_MODULE_LOCK_FORMAT,
            root: self.manifest.name.clone(),
            packages,
        })
    }

    fn collect_lock_entries(
        &self,
        root: &Path,
        output: &mut BTreeMap<String, LockedProgramModule>,
    ) -> Result<()> {
        let manifest = self
            .manifest_path
            .strip_prefix(root)
            .with_context(|| format!("package {} is outside module root", self.manifest.name))?
            .to_path_buf();
        let entry = LockedProgramModule {
            manifest,
            version: self.manifest.version.clone(),
            interface_version: self.manifest.interface_version,
            manifest_sha256: self.manifest_sha256.clone(),
            source_sha256: self.source_sha256.clone(),
        };
        if let Some(previous) = output.insert(self.manifest.name.clone(), entry.clone()) {
            if previous != entry {
                bail!(
                    "two different module packages use the name {:?}",
                    self.manifest.name
                );
            }
        }
        for dependency in self.dependencies.values() {
            dependency.collect_lock_entries(root, output)?;
        }
        Ok(())
    }
}

/// Load the full local package graph. The optional lock file is checked if
/// supplied; manifest SHA-256 pins remain mandatory for every dependency.
pub fn load_program_module_package(
    manifest_path: &Path,
    allowed_root: &Path,
    lock_path: Option<&Path>,
) -> Result<LoadedProgramModulePackage> {
    let root = canonical_directory(allowed_root, "module root")?;
    let mut state = LoadState {
        root: root.clone(),
        active: Vec::new(),
        loaded_packages: BTreeMap::new(),
        loaded_names: BTreeMap::new(),
        packages: 0,
        total_bytes: 0,
    };
    let mut loaded = load_package_inner(manifest_path, &mut state, 0)?;
    if let Some(lock_path) = lock_path {
        verify_lock(lock_path, &root, &loaded)?;
    }
    loaded.expanded_bytes = state.total_bytes;
    Ok(loaded)
}

/// Load the source selected by an applet manifest's `program` descriptor.
/// The source path is based at the declaring manifest's directory, never the
/// caller's working directory.
pub fn load_program_module(
    reference: &ProgramModule,
    applet_manifest_path: &Path,
    allowed_root: &Path,
) -> Result<LoadedProgramSource> {
    reference.validate()?;
    let root = canonical_directory(allowed_root, "module root")?;
    let applet_manifest_path = resolve_inside_root(applet_manifest_path, &root, "applet manifest")?;
    let source_path = resolve_relative_file(
        &applet_manifest_path,
        &reference.source,
        &root,
        "program module source",
    )?;
    let (
        program,
        definition,
        source_sha256,
        transitive_fingerprints,
        export,
        expanded_bytes,
        package,
    ) = match (&reference.package_manifest, &reference.package_lock) {
        (None, None) => {
            let source_bytes = read_limited(
                &source_path,
                MAX_PROGRAM_MODULE_GRAPH_BYTES,
                "program module source",
            )?;
            let source_sha256 = sha256_hex(&source_bytes);
            let source = std::str::from_utf8(&source_bytes).with_context(|| {
                format!(
                    "program module source {} is not UTF-8",
                    source_path.display()
                )
            })?;
            let program = parse_module_source(source, &source_path)?;
            let definition = program
                .definitions
                .get(&reference.entry)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "program module source does not export entry {:?}",
                        reference.entry
                    )
                })?;
            let expanded_bytes = source_bytes.len();
            (
                Arc::new(program),
                definition,
                source_sha256,
                Vec::new(),
                None,
                expanded_bytes,
                None,
            )
        }
        (Some(package_manifest), Some(package_lock)) => {
            let package_manifest_path = resolve_relative_file(
                &applet_manifest_path,
                package_manifest,
                &root,
                "program module package manifest",
            )?;
            let package_lock_path = resolve_relative_file(
                &applet_manifest_path,
                package_lock,
                &root,
                "program module package lock",
            )?;
            let package = load_program_module_package(
                &package_manifest_path,
                &root,
                Some(&package_lock_path),
            )?;
            let export = package.export(&reference.entry)?;
            if export.source_path != source_path {
                bail!(
                    "program module source {} does not match package export source {}",
                    source_path.display(),
                    export.source_path.display()
                );
            }
            if export.reference.interface_version != reference.interface_version {
                bail!(
                    "program module descriptor interface version {} does not match package export version {}",
                    reference.interface_version,
                    export.reference.interface_version
                );
            }
            (
                export.program,
                export.definition,
                export.source_sha256,
                export.transitive_fingerprints,
                Some(export.export),
                export.expanded_bytes,
                Some(Arc::new(package)),
            )
        }
        _ => unreachable!("ProgramModule::validate checks paired package paths"),
    };
    let checkpoint_fingerprint = checkpoint_fingerprint(
        reference,
        &source_path,
        &source_sha256,
        &transitive_fingerprints,
    )?;
    Ok(LoadedProgramSource {
        reference: reference.clone(),
        source_path,
        source_sha256,
        program,
        definition,
        export,
        transitive_fingerprints,
        expanded_bytes,
        package,
        checkpoint_fingerprint,
    })
}

fn parse_module_source(source: &str, source_path: &Path) -> Result<Program> {
    let program = crate::parser::parse(source)
        .with_context(|| format!("parsing program module source {}", source_path.display()))?;
    if program.trigger.is_some() {
        bail!("program module source cannot define a service trigger");
    }
    if !program.steps.is_empty() {
        bail!(
            "program module source may contain definitions only; top-level executable statements are not allowed"
        );
    }
    Ok(program)
}

fn checkpoint_fingerprint(
    reference: &ProgramModule,
    source_path: &Path,
    source_sha256: &str,
    transitive_fingerprints: &[ModuleFingerprint],
) -> Result<String> {
    let material = serde_json::to_vec(&(
        "program-module-checkpoint-v1",
        reference,
        source_path,
        source_sha256,
        transitive_fingerprints,
    ))?;
    Ok(sha256_hex(&material))
}

struct LoadState {
    root: PathBuf,
    active: Vec<PathBuf>,
    loaded_packages: BTreeMap<PathBuf, LoadedProgramModulePackage>,
    loaded_names: BTreeMap<String, PathBuf>,
    packages: usize,
    total_bytes: usize,
}

fn load_package_inner(
    manifest_path: &Path,
    state: &mut LoadState,
    depth: usize,
) -> Result<LoadedProgramModulePackage> {
    if depth >= MAX_PROGRAM_MODULE_DEPTH {
        bail!("program module dependency depth exceeds {MAX_PROGRAM_MODULE_DEPTH}");
    }
    let manifest_path = resolve_inside_root(manifest_path, &state.root, "module manifest")?;
    if state.active.contains(&manifest_path) {
        let cycle = state
            .active
            .iter()
            .chain(std::iter::once(&manifest_path))
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" -> ");
        bail!("program module dependency cycle: {cycle}");
    }
    if let Some(loaded) = state.loaded_packages.get(&manifest_path) {
        return Ok(loaded.clone());
    }
    state.packages += 1;
    if state.packages > MAX_PROGRAM_MODULE_PACKAGES {
        bail!("program module dependency graph exceeds {MAX_PROGRAM_MODULE_PACKAGES} packages");
    }
    state.active.push(manifest_path.clone());
    let result = (|| {
        let (manifest, manifest_bytes) = read_json_file::<ProgramModulePackageManifest>(
            &manifest_path,
            MAX_MODULE_MANIFEST_BYTES,
            "program module manifest",
        )?;
        charge_bytes(state, manifest_bytes.len())?;
        validate_package_manifest(&manifest)?;
        let manifest_sha256 = sha256_hex(&manifest_bytes);
        if let Some(previous) = state.loaded_names.get(&manifest.name) {
            if previous != &manifest_path {
                bail!(
                    "two different module manifests use the package name {:?}",
                    manifest.name
                );
            }
        } else {
            state
                .loaded_names
                .insert(manifest.name.clone(), manifest_path.clone());
        }

        let source_path = resolve_relative_file(
            &manifest_path,
            &manifest.source,
            &state.root,
            "program source",
        )?;
        let source_bytes = read_limited(
            &source_path,
            MAX_PROGRAM_MODULE_GRAPH_BYTES,
            "program source",
        )?;
        charge_bytes(state, source_bytes.len())?;
        let source_sha256 = sha256_hex(&source_bytes);
        if !source_sha256.eq_ignore_ascii_case(&manifest.source_sha256) {
            bail!(
                "program source SHA-256 mismatch for {}: expected {}, found {}",
                source_path.display(),
                manifest.source_sha256,
                source_sha256
            );
        }
        let source = std::str::from_utf8(&source_bytes)
            .with_context(|| format!("program source {} is not UTF-8", source_path.display()))?;
        let program = crate::parser::parse(source)
            .with_context(|| format!("parsing program module source {}", source_path.display()))?;
        if program.trigger.is_some() {
            bail!("program module source cannot define a service trigger");
        }
        if !program.steps.is_empty() {
            bail!(
                "program module source may contain definitions only; top-level executable statements are not allowed"
            );
        }
        for (export_name, export) in &manifest.exports {
            validate_export_declaration(export_name, export, &manifest)?;
            let definition = program.definitions.get(&export.entry).ok_or_else(|| {
                anyhow!(
                    "program module {:?} does not define exported entry {:?}",
                    manifest.name,
                    export.entry
                )
            })?;
            validate_export_inputs(export_name, &export.inputs, definition)?;
            validate_output_schema(&export.output)?;
        }

        let mut dependencies = BTreeMap::new();
        for (alias, dependency) in &manifest.dependencies {
            validate_dependency(alias, dependency)?;
            let child_path = resolve_relative_file(
                &manifest_path,
                &dependency.manifest,
                &state.root,
                "dependency manifest",
            )?;
            let child = load_package_inner(&child_path, state, depth + 1)
                .with_context(|| format!("loading program module dependency {alias:?}"))?;
            if !child
                .source_sha256
                .eq_ignore_ascii_case(&dependency.source_sha256)
            {
                bail!(
                    "dependency {alias:?} source SHA-256 mismatch: expected {}, found {}",
                    dependency.source_sha256,
                    child.source_sha256
                );
            }
            if !child
                .manifest_sha256
                .eq_ignore_ascii_case(&dependency.manifest_sha256)
            {
                bail!(
                    "dependency {alias:?} manifest SHA-256 mismatch: expected {}, found {}",
                    dependency.manifest_sha256,
                    child.manifest_sha256
                );
            }
            if !version_requirement_matches(&dependency.version, &child.manifest.version)? {
                bail!(
                    "dependency {alias:?} requires version {:?}, found {}",
                    dependency.version,
                    child.manifest.version
                );
            }
            if !(dependency.interface_min..dependency.interface_max_exclusive)
                .contains(&child.manifest.interface_version)
            {
                bail!(
                    "dependency {alias:?} requires interface version in {}..{}, found {}",
                    dependency.interface_min,
                    dependency.interface_max_exclusive,
                    child.manifest.interface_version
                );
            }
            dependencies.insert(alias.clone(), Arc::new(child));
        }

        let mut fingerprints = vec![ModuleFingerprint {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            interface_version: manifest.interface_version,
            manifest_sha256: manifest_sha256.clone(),
            source_sha256: source_sha256.clone(),
        }];
        for dependency in dependencies.values() {
            fingerprints.extend(dependency.fingerprints.iter().cloned());
        }
        fingerprints.sort_by(|left, right| left.name.cmp(&right.name));
        fingerprints.dedup_by(|left, right| left.name == right.name);

        Ok(LoadedProgramModulePackage {
            manifest_path: manifest_path.clone(),
            manifest_sha256,
            source_path,
            source_sha256,
            manifest,
            program: Arc::new(program),
            dependencies,
            expanded_bytes: 0,
            fingerprints,
        })
    })();
    state.active.pop();
    if let Ok(loaded) = &result {
        state.loaded_packages.insert(manifest_path, loaded.clone());
    }
    result
}

fn validate_package_manifest(manifest: &ProgramModulePackageManifest) -> Result<()> {
    if manifest.format_version != PROGRAM_MODULE_PACKAGE_FORMAT {
        bail!(
            "unsupported program module package format {}; supported format is {}",
            manifest.format_version,
            PROGRAM_MODULE_PACKAGE_FORMAT
        );
    }
    if !valid_package_name(&manifest.name) {
        bail!("invalid program module package name {:?}", manifest.name);
    }
    parse_semver(&manifest.version)?;
    if manifest.interface_version != PROGRAM_MODULE_INTERFACE_VERSION {
        bail!(
            "unsupported program module interface version {}; supported version is {}",
            manifest.interface_version,
            PROGRAM_MODULE_INTERFACE_VERSION
        );
    }
    if manifest.source.as_os_str().is_empty() || manifest.source.is_absolute() {
        bail!("program module source must be a nonempty relative path");
    }
    validate_sha256(&manifest.source_sha256, "source_sha256")?;
    if manifest.exports.is_empty() {
        bail!("program module package must declare at least one export");
    }
    Ok(())
}

fn validate_export_declaration(
    name: &str,
    export: &ProgramModuleExport,
    package: &ProgramModulePackageManifest,
) -> Result<()> {
    if !valid_identifier(name) || !valid_identifier(&export.entry) {
        bail!("program module export names and entries must be identifiers");
    }
    if export.interface_version != package.interface_version {
        bail!(
            "program module export {name:?} interface version {} does not match package version {}",
            export.interface_version,
            package.interface_version
        );
    }
    if export.lifecycle != ModuleLifecycle::OneShot {
        bail!(
            "program module export {name:?} uses unsupported {:?} lifecycle; finite callers require one_shot",
            export.lifecycle
        );
    }
    let mut input_names = BTreeSet::new();
    for input in &export.inputs {
        if !valid_identifier(&input.name) || !input_names.insert(&input.name) {
            bail!(
                "program module export {name:?} has an invalid or duplicate input name {:?}",
                input.name
            );
        }
    }
    let mut capabilities = BTreeSet::new();
    for capability in &export.capabilities {
        if !valid_capability_id(capability) || !capabilities.insert(capability.clone()) {
            bail!(
                "program module export {name:?} has an invalid or duplicate capability {capability:?}"
            );
        }
    }
    let effects = export.effects.iter().collect::<BTreeSet<_>>();
    if effects.len() != export.effects.len()
        || export
            .effects
            .iter()
            .any(|effect| !valid_capability_id(effect) || !capabilities.contains(effect))
    {
        bail!(
            "program module export {name:?} effects must be unique and included in its capabilities"
        );
    }
    let mut statuses = BTreeSet::new();
    let mut outcome_names = BTreeSet::new();
    let mut has_success = export.outcomes.is_empty();
    for outcome in &export.outcomes {
        if !statuses.insert(outcome.status)
            || !valid_outcome_name(&outcome.name)
            || !outcome_names.insert(&outcome.name)
        {
            bail!("program module export {name:?} has duplicate or invalid outcome declarations");
        }
        has_success |= outcome.kind == OutcomeKind::Success;
    }
    if !has_success {
        bail!("program module export {name:?} must declare a success outcome");
    }
    if export.result_envelope && export.output.kind == OutputType::Bytes {
        bail!("program module export {name:?} result_envelope requires a typed non-bytes output");
    }
    Ok(())
}

fn validate_export_inputs(
    export_name: &str,
    expected: &[ProgramModuleInput],
    definition: &ReusableProgram,
) -> Result<()> {
    if expected.len() != definition.inputs.len() {
        bail!(
            "program module export {export_name:?} declares {} inputs, source entry declares {}",
            expected.len(),
            definition.inputs.len()
        );
    }
    let source_inputs = definition
        .inputs
        .iter()
        .map(|input| (input.name.as_str(), input))
        .collect::<BTreeMap<_, _>>();
    if source_inputs.len() != definition.inputs.len() {
        bail!("program module entry {export_name:?} declares duplicate source inputs");
    }
    for contract in expected {
        let Some(source) = source_inputs.get(contract.name.as_str()) else {
            bail!(
                "program module export {export_name:?} declares input {:?} missing from source entry",
                contract.name
            );
        };
        if InputType::from(contract.kind) != source.kind
            || contract.required != source.default.is_none()
        {
            bail!(
                "program module export {export_name:?} input {:?} does not match its source declaration",
                contract.name
            );
        }
    }
    Ok(())
}

fn validate_output_schema(output: &OutputSchema) -> Result<()> {
    output
        .validate()
        .context("invalid program module output schema")?;
    if output.kind == OutputType::Bytes && output.encoding != crate::contracts::OutputEncoding::Raw
    {
        bail!("program module bytes output requires raw encoding");
    }
    Ok(())
}

fn validate_dependency(alias: &str, dependency: &ProgramModuleDependency) -> Result<()> {
    if !valid_identifier(alias) {
        bail!("program module dependency alias {alias:?} must be an identifier");
    }
    if dependency.manifest.as_os_str().is_empty() || dependency.manifest.is_absolute() {
        bail!("program module dependency manifest must be a nonempty relative path");
    }
    validate_sha256(&dependency.source_sha256, "dependency source_sha256")?;
    validate_sha256(&dependency.manifest_sha256, "dependency manifest_sha256")?;
    let (operator, version) = parse_version_requirement(&dependency.version)?;
    let _ = (operator, version);
    if dependency.interface_min >= dependency.interface_max_exclusive {
        bail!("module interface range must have a lower bound below its exclusive upper bound");
    }
    Ok(())
}

fn verify_lock(lock_path: &Path, root: &Path, loaded: &LoadedProgramModulePackage) -> Result<()> {
    let lock_path = resolve_inside_root(lock_path, root, "program module lock")?;
    let (lock, _) = read_json_file::<ProgramModuleLock>(
        &lock_path,
        MAX_MODULE_MANIFEST_BYTES,
        "program module lock",
    )?;
    if lock.lock_version != PROGRAM_MODULE_LOCK_FORMAT {
        bail!(
            "unsupported program module lock version {}; supported version is {}",
            lock.lock_version,
            PROGRAM_MODULE_LOCK_FORMAT
        );
    }
    if lock.root != loaded.manifest.name {
        bail!(
            "program module lock root does not match package {:?}",
            loaded.manifest.name
        );
    }
    let expected = loaded.lock_snapshot(root)?;
    if lock.packages != expected.packages {
        bail!(
            "program module lock does not match the resolved local package graph; regenerate it explicitly"
        );
    }
    Ok(())
}

fn resolve_relative_file(
    manifest_path: &Path,
    relative: &Path,
    root: &Path,
    what: &str,
) -> Result<PathBuf> {
    if relative.is_absolute() {
        bail!("{what} path must be relative to its declaring manifest");
    }
    let parent = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", manifest_path.display()))?;
    resolve_inside_root(&parent.join(relative), root, what)
}

fn resolve_inside_root(path: &Path, root: &Path, what: &str) -> Result<PathBuf> {
    if path
        .components()
        .any(|component| matches!(component, Component::Prefix(_)))
    {
        bail!("{what} path has an unsupported prefix");
    }
    let canonical =
        fs::canonicalize(path).with_context(|| format!("resolving {what} {}", path.display()))?;
    if !canonical.starts_with(root) {
        bail!(
            "{what} {} resolves outside allowed module root {}",
            path.display(),
            root.display()
        );
    }
    let metadata = fs::metadata(&canonical).with_context(|| format!("reading {what} metadata"))?;
    if !metadata.is_file() {
        bail!("{what} {} is not a regular file", canonical.display());
    }
    Ok(canonical)
}

fn canonical_directory(path: &Path, what: &str) -> Result<PathBuf> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("resolving {what} {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{what} {} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn read_json_file<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum: usize,
    what: &str,
) -> Result<(T, Vec<u8>)> {
    let bytes = read_limited(path, maximum, what)?;
    let value = serde_json::from_slice(&bytes)
        .with_context(|| format!("decoding {what} {}", path.display()))?;
    Ok((value, bytes))
}

fn read_limited(path: &Path, maximum: usize, what: &str) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path).with_context(|| format!("reading {what} metadata"))?;
    if !metadata.is_file() {
        bail!("{what} {} is not a regular file", path.display());
    }
    if metadata.len() > maximum as u64 {
        bail!("{what} {} exceeds {maximum} bytes", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("reading {what} {}", path.display()))?;
    if bytes.len() > maximum {
        bail!("{what} {} exceeds {maximum} bytes", path.display());
    }
    Ok(bytes)
}

fn charge_bytes(state: &mut LoadState, amount: usize) -> Result<()> {
    state.total_bytes = state
        .total_bytes
        .checked_add(amount)
        .ok_or_else(|| anyhow!("program module package graph size overflow"))?;
    if state.total_bytes > MAX_PROGRAM_MODULE_GRAPH_BYTES {
        bail!("program module package graph exceeds {MAX_PROGRAM_MODULE_GRAPH_BYTES} bytes");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_sha256(value: &str, description: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{description} must be a 64-character SHA-256 hex digest");
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !matches!(value, "true" | "false")
}

fn valid_package_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
}

fn valid_capability_id(value: &str) -> bool {
    let mut segments = value.split('.');
    let first = segments.next().unwrap_or_default();
    if first.is_empty()
        || !first.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
    {
        return false;
    }
    let mut has_namespace = false;
    for segment in segments {
        has_namespace = true;
        if segment.is_empty()
            || !segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            })
        {
            return false;
        }
    }
    has_namespace
}

fn valid_outcome_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<PrereleaseIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrereleaseIdentifier {
    Numeric(u64),
    AlphaNumeric(String),
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(
                || match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self.prerelease.cmp(&other.prerelease),
                },
            )
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrereleaseIdentifier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        use PrereleaseIdentifier::{AlphaNumeric, Numeric};
        match (self, other) {
            (Numeric(left), Numeric(right)) => left.cmp(right),
            (Numeric(_), AlphaNumeric(_)) => Ordering::Less,
            (AlphaNumeric(_), Numeric(_)) => Ordering::Greater,
            (AlphaNumeric(left), AlphaNumeric(right)) => left.cmp(right),
        }
    }
}

impl PartialOrd for PrereleaseIdentifier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_semver(value: &str) -> Result<SemVer> {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(a, b)| (a, Some(b)));
    if let Some(build) = build {
        validate_identifiers(build, false)
            .with_context(|| format!("invalid SemVer build metadata {value:?}"))?;
    }
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(a, b)| (a, Some(b)));
    let mut core_parts = core.split('.');
    let major = parse_semver_number(core_parts.next(), "major", value)?;
    let minor = parse_semver_number(core_parts.next(), "minor", value)?;
    let patch = parse_semver_number(core_parts.next(), "patch", value)?;
    if core_parts.next().is_some() {
        bail!("invalid SemVer version {value:?}");
    }
    let prerelease = match prerelease {
        None => Vec::new(),
        Some(text) => {
            if text.is_empty() {
                bail!("invalid SemVer prerelease in {value:?}");
            }
            text.split('.')
                .map(|part| {
                    if part.is_empty()
                        || !part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    {
                        bail!("invalid SemVer prerelease in {value:?}");
                    }
                    if part.bytes().all(|byte| byte.is_ascii_digit()) {
                        if part.len() > 1 && part.starts_with('0') {
                            bail!("numeric SemVer prerelease identifiers cannot have leading zeroes: {value:?}");
                        }
                        Ok(PrereleaseIdentifier::Numeric(part.parse().with_context(|| format!("invalid SemVer prerelease number in {value:?}"))?))
                    } else {
                        Ok(PrereleaseIdentifier::AlphaNumeric(part.to_owned()))
                    }
                })
                .collect::<Result<Vec<_>>>()?
        }
    };
    Ok(SemVer {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn parse_semver_number(value: Option<&str>, part: &str, full: &str) -> Result<u64> {
    let value =
        value.ok_or_else(|| anyhow!("SemVer version must include major.minor.patch: {full:?}"))?;
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("invalid SemVer {part} component in {full:?}");
    }
    value
        .parse()
        .with_context(|| format!("SemVer {part} component is too large in {full:?}"))
}

fn validate_identifiers(value: &str, no_leading_zero: bool) -> Result<()> {
    if value.is_empty() {
        bail!("SemVer identifiers cannot be empty");
    }
    for part in value.split('.') {
        if part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || (no_leading_zero
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && part.len() > 1
                && part.starts_with('0'))
        {
            bail!("invalid SemVer identifier {part:?}");
        }
    }
    Ok(())
}

fn parse_version_requirement(value: &str) -> Result<(bool, SemVer)> {
    let (caret, version) = match value.strip_prefix('^') {
        Some(version) => (true, version),
        None => (false, value.strip_prefix('=').unwrap_or(value)),
    };
    if version.is_empty() || version.contains(' ') || version.contains(',') {
        bail!(
            "unsupported SemVer requirement {value:?}; use an exact version or a caret range such as ^1.2.3"
        );
    }
    Ok((caret, parse_semver(version)?))
}

fn version_requirement_matches(requirement: &str, candidate: &str) -> Result<bool> {
    let (caret, lower) = parse_version_requirement(requirement)?;
    let candidate = parse_semver(candidate)?;
    if !caret {
        return Ok(candidate == lower);
    }
    if candidate < lower {
        return Ok(false);
    }
    // SemVer range matching excludes prereleases unless the range explicitly
    // names a prerelease with the same major/minor/patch tuple.
    if !candidate.prerelease.is_empty()
        && (lower.prerelease.is_empty()
            || (candidate.major, candidate.minor, candidate.patch)
                != (lower.major, lower.minor, lower.patch))
    {
        return Ok(false);
    }
    let upper = if lower.major > 0 {
        lower.major.checked_add(1).map(|major| SemVer {
            major,
            minor: 0,
            patch: 0,
            prerelease: Vec::new(),
        })
    } else if lower.minor > 0 {
        lower.minor.checked_add(1).map(|minor| SemVer {
            major: 0,
            minor,
            patch: 0,
            prerelease: Vec::new(),
        })
    } else {
        lower.patch.checked_add(1).map(|patch| SemVer {
            major: 0,
            minor: 0,
            patch,
            prerelease: Vec::new(),
        })
    };
    Ok(upper.is_none_or(|upper| candidate < upper))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{OutputEncoding, ValueSchema};
    use tempfile::tempdir;

    fn schema() -> OutputSchema {
        OutputSchema {
            kind: OutputType::Record,
            encoding: OutputEncoding::Json,
            items: None,
            fields: BTreeMap::from([
                (
                    "text".into(),
                    ValueSchema {
                        kind: OutputType::Text,
                        nullable: false,
                        items: None,
                        fields: BTreeMap::new(),
                        required: Vec::new(),
                    },
                ),
                (
                    "length".into(),
                    ValueSchema {
                        kind: OutputType::Integer,
                        nullable: false,
                        items: None,
                        fields: BTreeMap::new(),
                        required: Vec::new(),
                    },
                ),
            ]),
            required: vec!["text".into(), "length".into()],
            trim_final_lf: false,
        }
    }

    fn manifest(source: &str) -> ProgramModulePackageManifest {
        ProgramModulePackageManifest {
            format_version: PROGRAM_MODULE_PACKAGE_FORMAT,
            name: "example.text".into(),
            version: "1.2.3".into(),
            interface_version: PROGRAM_MODULE_INTERFACE_VERSION,
            source: "src/library.program".into(),
            source_sha256: sha256_hex(source.as_bytes()),
            exports: BTreeMap::from([(
                "describe".into(),
                ProgramModuleExport {
                    entry: "describe".into(),
                    interface_version: PROGRAM_MODULE_INTERFACE_VERSION,
                    inputs: vec![ProgramModuleInput {
                        name: "text".into(),
                        kind: ProgramModuleInputType::Text,
                        required: true,
                    }],
                    output: schema(),
                    outcomes: Vec::new(),
                    result_envelope: false,
                    lifecycle: ModuleLifecycle::OneShot,
                    capabilities: Vec::new(),
                    effects: Vec::new(),
                },
            )]),
            dependencies: BTreeMap::new(),
        }
    }

    fn write_package(dir: &Path, source: &str) -> PathBuf {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/library.program"), source).unwrap();
        let manifest = manifest(source);
        let path = dir.join("program.modules.json");
        fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        path
    }

    #[test]
    fn package_load_is_manifest_relative_and_exports_typed_record_outputs() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let package_dir = root.join("example");
        let source = "define a program named describe\n  accept text as text\n  run \"printf\" with arguments [\"{\\\"text\\\":\\\"%s\\\",\\\"length\\\":%s}\", text, 4]\n";
        let manifest_path = write_package(&package_dir, source);
        let loaded = load_program_module_package(&manifest_path, &root, None).unwrap();
        let export = loaded.export("describe").unwrap();
        assert_eq!(
            export.source_path,
            fs::canonicalize(package_dir.join("src/library.program")).unwrap()
        );
        assert_eq!(export.definition.inputs[0].name, "text");
        assert_eq!(export.export.output.kind, OutputType::Record);
        assert_eq!(export.export.output.fields.len(), 2);
        assert_eq!(export.reference.interface_version, 1);
    }

    #[test]
    fn manifest_hash_and_entry_are_checked_before_a_module_can_be_loaded() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let package_dir = root.join("example");
        let manifest_path = write_package(
            &package_dir,
            "define a program named describe\n  accept text as text\n  run \"true\"\n",
        );
        let loaded = load_program_module_package(&manifest_path, &root, None).unwrap();
        assert!(loaded
            .export("missing")
            .unwrap_err()
            .to_string()
            .contains("does not export"));
        fs::write(package_dir.join("src/library.program"), "run \"false\"\n").unwrap();
        let error = load_program_module_package(&manifest_path, &root, None).unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256 mismatch"));
    }

    #[test]
    fn interface_and_unsupported_lifecycle_fail_closed() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let package_dir = root.join("example");
        let manifest_path = write_package(
            &package_dir,
            "define a program named describe\n  accept text as text\n  run \"true\"\n",
        );
        let mut package: ProgramModulePackageManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        package.interface_version = 2;
        fs::write(&manifest_path, serde_json::to_vec(&package).unwrap()).unwrap();
        let error = load_program_module_package(&manifest_path, &root, None).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported program module interface version"));

        package.interface_version = 1;
        package.exports.get_mut("describe").unwrap().lifecycle = ModuleLifecycle::Listener;
        fs::write(&manifest_path, serde_json::to_vec(&package).unwrap()).unwrap();
        let error = load_program_module_package(&manifest_path, &root, None).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported Listener lifecycle"));
    }

    #[test]
    fn lock_snapshot_pins_manifest_and_transitive_source_hashes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let package_dir = root.join("example");
        let manifest_path = write_package(
            &package_dir,
            "define a program named describe\n  accept text as text\n  run \"true\"\n",
        );
        let loaded = load_program_module_package(&manifest_path, &root, None).unwrap();
        let lock_path = root.join("program.modules.lock.json");
        fs::write(
            &lock_path,
            serde_json::to_vec_pretty(&loaded.lock_snapshot(&root).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(load_program_module_package(&manifest_path, &root, Some(&lock_path)).is_ok());
        fs::write(package_dir.join("src/library.program"), "\n").unwrap();
        let error =
            load_program_module_package(&manifest_path, &root, Some(&lock_path)).unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256 mismatch"));
    }

    #[test]
    fn dependency_pins_are_local_and_cycles_are_rejected() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        let first_source =
            "define a program named describe\n  accept text as text\n  run \"true\"\n";
        let second_source =
            "define a program named describe\n  accept text as text\n  run \"true\"\n";
        let first_path = write_package(&first_dir, first_source);
        let second_path = write_package(&second_dir, second_source);
        let first_raw = fs::read(&first_path).unwrap();
        let mut first: ProgramModulePackageManifest = serde_json::from_slice(&first_raw).unwrap();
        first.name = "example.first".into();
        first.dependencies.insert(
            "second".into(),
            ProgramModuleDependency {
                manifest: "../second/program.modules.json".into(),
                source_sha256: sha256_hex(second_source.as_bytes()),
                manifest_sha256: "0".repeat(64),
                version: "^1.2.0".into(),
                interface_min: 1,
                interface_max_exclusive: 2,
            },
        );
        fs::write(&first_path, serde_json::to_vec(&first).unwrap()).unwrap();
        let second_raw = fs::read(&second_path).unwrap();
        let mut second: ProgramModulePackageManifest = serde_json::from_slice(&second_raw).unwrap();
        second.name = "example.second".into();
        second.version = "1.2.4".into();
        second.dependencies.insert(
            "first".into(),
            ProgramModuleDependency {
                manifest: "../first/program.modules.json".into(),
                source_sha256: sha256_hex(first_source.as_bytes()),
                manifest_sha256: "0".repeat(64),
                version: "^1.2.0".into(),
                interface_min: 1,
                interface_max_exclusive: 2,
            },
        );
        fs::write(&second_path, serde_json::to_vec(&second).unwrap()).unwrap();
        let error = load_program_module_package(&second_path, &root, None).unwrap_err();
        assert!(format!("{error:#}").contains("program module dependency cycle"));
    }

    #[test]
    fn semver_caret_ranges_use_semver_precedence_and_bounds() {
        assert!(version_requirement_matches("^1.2.3", "1.9.0").unwrap());
        assert!(!version_requirement_matches("^1.2.3", "2.0.0").unwrap());
        assert!(version_requirement_matches("^0.2.3", "0.2.9").unwrap());
        assert!(!version_requirement_matches("^0.2.3", "0.3.0").unwrap());
        assert!(!version_requirement_matches("^1.2.3", "1.2.3-beta.1").unwrap());
        assert!(version_requirement_matches("^1.2.3-beta.1", "1.2.3-beta.2").unwrap());
        assert!(!version_requirement_matches("^1.2.3-beta.1", "1.2.4-beta.1").unwrap());
        assert!(version_requirement_matches("^1.2.3-beta.1", "1.2.3").unwrap());
        assert!(version_requirement_matches("=1.2.3+local", "1.2.3+other").unwrap());
        assert!(parse_semver("01.2.3").is_err());
        assert!(parse_version_requirement(">=1.0.0, <2.0.0").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_resolving_outside_the_allowed_root_are_rejected() {
        use std::os::unix::fs::symlink;
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let outside = temp.path().join("outside.program");
        fs::create_dir_all(root.join("example/src")).unwrap();
        fs::write(
            &outside,
            "define a program named describe\n  accept text as text\n  run \"true\"\n",
        )
        .unwrap();
        symlink(&outside, root.join("example/src/library.program")).unwrap();
        let package = manifest("define a program named describe\n  run \"true\"\n");
        let path = root.join("example/program.modules.json");
        fs::write(&path, serde_json::to_vec(&package).unwrap()).unwrap();
        let error = load_program_module_package(&path, &root, None).unwrap_err();
        assert!(format!("{error:#}").contains("outside allowed module root"));
    }

    #[test]
    fn package_manifest_rejects_bad_hash_shape_and_semver() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let package_dir = root.join("example");
        let path = write_package(
            &package_dir,
            "define a program named describe\n  run \"true\"\n",
        );
        let mut package: ProgramModulePackageManifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        package.version = "1.02.3".into();
        fs::write(&path, serde_json::to_vec(&package).unwrap()).unwrap();
        assert!(load_program_module_package(&path, &root, None).is_err());
    }

    #[test]
    fn direct_module_descriptor_requires_relative_path_and_known_interface() {
        assert!(ProgramModule {
            source: PathBuf::from("modules/clean.program"),
            entry: "clean".into(),
            interface_version: 1,
            package_manifest: None,
            package_lock: None,
        }
        .validate()
        .is_ok());
        assert!(ProgramModule {
            source: PathBuf::from("/tmp/clean.program"),
            entry: "clean".into(),
            interface_version: 1,
            package_manifest: None,
            package_lock: None,
        }
        .validate()
        .is_err());
        assert!(ProgramModule {
            source: PathBuf::from("clean.program"),
            entry: "clean".into(),
            interface_version: 2,
            package_manifest: None,
            package_lock: None,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn direct_module_source_resolves_from_manifest_origin_and_selects_export() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("packages");
        fs::create_dir_all(root.join("applets")).unwrap();
        fs::create_dir_all(root.join("modules")).unwrap();
        let source = "define a program named normalize\n  accept text as text\n  run \"printf\" with arguments [\"%s\", text]\n";
        fs::write(root.join("modules/text.program"), source).unwrap();
        let applet_manifest_path = root.join("applets/normalize.json");
        fs::write(&applet_manifest_path, "{}").unwrap();
        let loaded = load_program_module(
            &ProgramModule {
                source: PathBuf::from("../modules/text.program"),
                entry: "normalize".into(),
                interface_version: 1,
                package_manifest: None,
                package_lock: None,
            },
            &applet_manifest_path,
            &root,
        )
        .unwrap();
        assert_eq!(loaded.definition.inputs[0].name, "text");
        assert_eq!(loaded.source_sha256, sha256_hex(source.as_bytes()));
        assert!(load_program_module(
            &ProgramModule {
                source: PathBuf::from("../modules/text.program"),
                entry: "missing".into(),
                interface_version: 1,
                package_manifest: None,
                package_lock: None,
            },
            &applet_manifest_path,
            &root,
        )
        .unwrap_err()
        .to_string()
        .contains("does not export entry"));
    }

    #[test]
    fn lock_and_loader_do_not_modify_package_files() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("modules");
        let package_dir = root.join("example");
        let manifest_path = write_package(
            &package_dir,
            "define a program named describe\n  accept text as text\n  run \"true\"\n",
        );
        let manifest_before = fs::read(&manifest_path).unwrap();
        let source_before = fs::read(package_dir.join("src/library.program")).unwrap();
        let loaded = load_program_module_package(&manifest_path, &root, None).unwrap();
        let lock = serde_json::to_vec_pretty(&loaded.lock_snapshot(&root).unwrap()).unwrap();
        assert!(!lock.is_empty());
        assert_eq!(fs::read(&manifest_path).unwrap(), manifest_before);
        assert_eq!(
            fs::read(package_dir.join("src/library.program")).unwrap(),
            source_before
        );
    }
}
