use crate::ast::{InputType, Invocation, Value};
use crate::bundled_manifests;
pub use crate::contracts::{
    AppletBackend, AppletContract, AppletEnvironment, AppletOutput, AppletPort, AppletPorts,
    Capability, Effect, EffectCertainty, Lifecycle, Outcome, OutcomeKind, OutcomeSpec,
    OutputEncoding, OutputSchema, OutputType, PortDelivery, ProgramModule, RemoteAuthSource,
    RemoteEndpointConfig, Sensitivity, StdioContract, StdioDisposition, ValueSchema,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::any::{Any, TypeId};
use std::borrow::{Borrow, Cow};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// A declarative description of an applet. Argument nodes produce argv elements
/// directly; no shell is involved in expanding a manifest.
#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub name: String,
    pub binary: String,
    pub builtin: Option<Builtin>,
    pub accepts_stdin: bool,
    pub cacheable: bool,
    /// Relative dependency paths whose contents participate in cache identity.
    /// The runtime resolves these against the invocation working directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cache_dependencies: Vec<String>,
    /// A failed attempt may already have caused an external effect, such as billing.
    #[serde(skip_serializing_if = "is_false")]
    pub uncertain_on_failure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    contract: Option<AppletContract>,
    /// The gated build feature this applet needs, when its backend alone does not say so.
    /// Remote-backed applets need `remote` whether or not they declare it.
    #[serde(skip_serializing_if = "Option::is_none")]
    build_feature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdio: Option<StdioContract>,
    /// Canonical source path for file-loaded manifests; embedded manifests
    /// have no origin. This runtime identity is excluded from serialization.
    #[serde(skip)]
    origin: Option<PathBuf>,
    pub phrases: Vec<String>,
    parameters: BTreeMap<String, ParameterSpec>,
    arguments: Vec<ArgumentNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    Cd,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawParameter")]
struct ParameterSpec {
    kind: ParameterType,
    item_type: Option<ParameterType>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    min_items: Option<usize>,
    /// For string lists: the first item's first word must be one of these
    /// (compared ASCII case-insensitively). Empty means no restriction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allow_first_token: Vec<String>,
    /// For string lists: the first item's first word must not be one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deny_first_token: Vec<String>,
    /// For string lists: every item must be safe to pass to a tool that reads
    /// options and `;`-separated command batches. Items starting with `-` are
    /// refused unless they are plain numbers, and separators or control bytes
    /// are refused everywhere.
    #[serde(default, skip_serializing_if = "is_false")]
    forbid_option_items: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fields: Option<BTreeMap<String, ParameterFieldSpec>>,
}

impl From<RawParameter> for ParameterSpec {
    fn from(raw: RawParameter) -> Self {
        Self {
            kind: raw.kind,
            item_type: raw.items,
            required: raw.required,
            min_items: raw.min_items,
            allow_first_token: raw.allow_first_token,
            deny_first_token: raw.deny_first_token,
            forbid_option_items: raw.forbid_option_items,
            fields: raw.fields,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawParameterField")]
struct ParameterFieldSpec {
    kind: ParameterType,
    required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fields: Option<BTreeMap<String, ParameterFieldSpec>>,
}

impl From<RawParameterField> for ParameterFieldSpec {
    fn from(raw: RawParameterField) -> Self {
        Self {
            kind: raw.kind,
            required: raw.required,
            fields: raw.fields,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum ParameterType {
    String,
    Path,
    Integer,
    Boolean,
    List,
    Json,
    Record,
    #[serde(skip_deserializing)]
    Scalar,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ArgumentNode {
    Literal {
        literal: String,
    },
    Parameter {
        parameter: String,
    },
    Spread {
        spread: String,
    },
    JsonParameter {
        json_parameter: String,
    },
    JsonArguments,
    /// A literal prefix followed by one path-like parameter, rendered as one
    /// argument. The value must be an absolute path free of address
    /// metacharacters (see `check_prefixed_path`), so the prefix chooses the
    /// address family and the parameter cannot change it.
    PrefixedParameter {
        prefix: String,
        parameter: String,
    },
    WhenPresent {
        when_present: String,
        append: Vec<ArgumentNode>,
    },
}

/// Loaded manifest catalog. `resolve` accepts either a canonical name or phrase.
#[derive(Clone, Default)]
pub struct Registry {
    data: Arc<RegistryData>,
}

#[derive(Default)]
struct RegistryData {
    // Shared between a registry and its detached copies: the parsed manifests and their names
    // are immutable once a registry is built, so a copy shares them and copies only the rest.
    manifests: Arc<Manifests>,
    names: Arc<BTreeMap<String, usize>>,
    module_root: Option<PathBuf>,
    scope_identity: Option<String>,
    host_state: Arc<Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
}

/// The manifests of one registry, in resolution order. A file load or an extension
/// keeps them parsed. A bundled registry shares the immutable build-time index and
/// parses each entry into its own cell on first use, with origin under `root`.
#[derive(Clone)]
enum Manifests {
    Parsed(Vec<Manifest>),
    Bundled {
        index: &'static bundled_manifests::BundledIndex,
        root: PathBuf,
        cells: Vec<OnceLock<Manifest>>,
        /// Whether each entry's file still has its indexed size, checked on first use.
        verified: Vec<OnceLock<bool>>,
    },
}

/// How much of the default manifest directory is compared with the bundled index
/// before a registry is built from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestCheck {
    /// File names at load time; each file's size when its manifest is first used. A
    /// changed file then fails closed: the registry never switches to disk content
    /// after it has handed manifests out, so preflight and execution see the same set.
    Lazy,
    /// Every file's size at load time. Any difference selects the full load from the
    /// directory, with its errors. `execute check` and applet listing use this.
    Full,
}

impl ManifestCheck {
    /// `check` compares the whole directory; every other command checks lazily.
    pub fn for_command(command: &str) -> Self {
        if command == "check" {
            Self::Full
        } else {
            Self::Lazy
        }
    }
}

impl Default for Manifests {
    fn default() -> Self {
        Self::Parsed(Vec::new())
    }
}

/// Whether the file for a bundled entry is still a plain file of the indexed size.
fn entry_matches(root: &Path, entry: &bundled_manifests::BundledEntry) -> bool {
    fs::metadata(root.join(entry.file()))
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == entry.bytes())
}

impl Manifests {
    fn len(&self) -> usize {
        match self {
            Self::Parsed(list) => list.len(),
            Self::Bundled { index, .. } => index.entries().len(),
        }
    }

    fn get(&self, position: usize) -> Result<&Manifest> {
        match self {
            Self::Parsed(list) => list
                .get(position)
                .ok_or_else(|| anyhow!("manifest {position} is out of range")),
            Self::Bundled {
                index,
                root,
                cells,
                verified,
            } => {
                let cell = cells
                    .get(position)
                    .ok_or_else(|| anyhow!("manifest {position} is out of range"))?;
                if let Some(manifest) = cell.get() {
                    return Ok(manifest);
                }
                let entry = &index.entries()[position];
                if !verified[position].get_or_init(|| entry_matches(root, entry)) {
                    bail!(
                        "manifest {} in {} differs from the bundled copy, so it was not loaded; \
                         run `execute check` to validate the directory, or pass --manifest-dir {} \
                         to load it as files",
                        entry.file(),
                        root.display(),
                        root.display()
                    );
                }
                let mut manifest = parse_manifest(index.body(position)?)
                    .with_context(|| format!("invalid bundled manifest {}", entry.file()))?;
                manifest.origin = Some(root.join(entry.file()));
                // A concurrent first use may have set the cell already; both values are equal.
                let _ = cell.set(manifest);
                cell.get()
                    .ok_or_else(|| anyhow!("bundled manifest {} was not stored", entry.file()))
            }
        }
    }

    fn all(&self) -> Result<Vec<&Manifest>> {
        (0..self.len()).map(|position| self.get(position)).collect()
    }

    fn to_parsed(&self) -> Result<Vec<Manifest>> {
        self.all()?
            .into_iter()
            .map(|manifest| Ok(manifest.clone()))
            .collect()
    }
}

/// Gated build features a manifest may declare. Each names a Cargo feature.
const KNOWN_BUILD_FEATURES: &[&str] = &["desktop", "remote"];

const MANIFEST_CATALOG_FORMAT: &str = "execute.manifest-catalog";
const MANIFEST_CATALOG_VERSION: u32 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifestCatalog<'a> {
    format: String,
    version: u32,
    #[serde(borrow)]
    manifests: Vec<&'a RawValue>,
}

impl Registry {
    /// Missing optional catalogs do not prevent direct executable calls.
    /// Explicit catalog paths should use `load` so configuration mistakes fail.
    pub fn load_optional(path: &Path) -> Result<Self> {
        match fs::metadata(path) {
            Ok(_) => Self::load(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("reading manifest path {}", path.display()))
            }
        }
    }

    pub fn resolve_call(&self, call: &Invocation) -> Result<&Manifest> {
        if !call.direct {
            return self.resolve(&call.operation);
        }
        if call.operation.is_empty() || call.operation.contains('\0') {
            bail!("executable name must be nonempty and cannot contain a NUL byte");
        }
        static DIRECT: OnceLock<Manifest> = OnceLock::new();
        Ok(DIRECT.get_or_init(|| Manifest {
            name: "direct executable".into(),
            binary: "".into(),
            builtin: None,
            accepts_stdin: true,
            cacheable: false,
            cache_dependencies: Vec::new(),
            uncertain_on_failure: false,
            contract: None,
            stdio: None,
            origin: None,
            build_feature: None,
            phrases: Vec::new(),
            parameters: BTreeMap::from([(
                "arguments".into(),
                ParameterSpec {
                    kind: ParameterType::List,
                    item_type: Some(ParameterType::Scalar),
                    required: false,
                    min_items: None,
                    allow_first_token: Vec::new(),
                    deny_first_token: Vec::new(),
                    forbid_option_items: false,
                    fields: None,
                },
            )]),
            arguments: vec![ArgumentNode::WhenPresent {
                when_present: "arguments".into(),
                append: vec![ArgumentNode::Spread {
                    spread: "arguments".into(),
                }],
            }],
        }))
    }

    /// Load one JSON manifest file or all `.json` files in a directory.
    pub fn load(path: &Path) -> Result<Self> {
        // Each entry records whether it is a plain file (not a symlink or a
        // directory). Such a file inside the canonical directory is its own
        // canonical path, so it needs no separate canonicalize call.
        let paths = if path.is_dir() {
            let mut entries = fs::read_dir(path)
                .with_context(|| format!("reading manifest directory {}", path.display()))?
                .map(|entry| {
                    entry.map(|e| {
                        let plain = e.file_type().is_ok_and(|kind| kind.is_file());
                        (e.path(), plain)
                    })
                })
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.retain(|(p, _)| p.extension().is_some_and(|ext| ext == "json"));
            entries.sort();
            entries
        } else {
            vec![(path.to_path_buf(), false)]
        };
        if paths.is_empty() {
            bail!("no JSON manifests found in {}", path.display());
        }

        let canonical_dir = if path.is_dir() {
            Some(
                fs::canonicalize(path)
                    .with_context(|| format!("canonicalizing manifest root {}", path.display()))?,
            )
        } else {
            None
        };
        let module_root = Some(match &canonical_dir {
            Some(root) => root.clone(),
            None => fs::canonicalize(path)
                .with_context(|| format!("canonicalizing manifest {}", path.display()))?
                .parent()
                .ok_or_else(|| anyhow!("manifest path has no parent directory"))?
                .to_path_buf(),
        });
        let mut data = RegistryData {
            module_root,
            ..RegistryData::default()
        };
        for (path, plain) in paths {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("reading manifest {}", path.display()))?;
            let origin = match (&canonical_dir, path.file_name(), plain) {
                (Some(root), Some(name), true) => root.join(name),
                _ => fs::canonicalize(&path)
                    .with_context(|| format!("canonicalizing manifest {}", path.display()))?,
            };
            Self::insert_source(&mut data, &raw, Some(origin))
                .with_context(|| format!("invalid manifest {}", path.display()))?;
        }
        Ok(Self {
            data: Arc::new(data),
        })
    }

    /// Load one explicit, versioned catalog file containing an ordered array
    /// of manifest objects. Every entry receives the catalog's canonical path
    /// as its origin, and its parent directory is the module root. The whole
    /// envelope's JSON syntax is parsed first; semantic manifest validation
    /// then runs in array order. A later syntax error therefore takes
    /// precedence over an earlier semantic manifest error.
    pub fn load_catalog(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading manifest catalog {}", path.display()))?;
        let origin = fs::canonicalize(path)
            .with_context(|| format!("canonicalizing manifest catalog {}", path.display()))?;
        let module_root = origin
            .parent()
            .ok_or_else(|| anyhow!("manifest catalog path has no parent directory"))?
            .to_path_buf();
        let catalog: RawManifestCatalog<'_> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing manifest catalog {}", path.display()))?;
        if catalog.format != MANIFEST_CATALOG_FORMAT {
            bail!(
                "unsupported manifest catalog format {:?}; expected {:?}",
                catalog.format,
                MANIFEST_CATALOG_FORMAT
            );
        }
        if catalog.version != MANIFEST_CATALOG_VERSION {
            bail!(
                "unsupported manifest catalog version {}; supported version is {}",
                catalog.version,
                MANIFEST_CATALOG_VERSION
            );
        }
        if catalog.manifests.is_empty() {
            bail!("manifest catalog {} contains no manifests", path.display());
        }

        let mut data = RegistryData {
            module_root: Some(module_root),
            ..RegistryData::default()
        };
        for (index, manifest) in catalog.manifests.iter().enumerate() {
            Self::insert_source(&mut data, manifest.get(), Some(origin.clone())).with_context(
                || {
                    format!(
                        "invalid manifest catalog entry {index} in {}",
                        path.display()
                    )
                },
            )?;
        }
        Ok(Self {
            data: Arc::new(data),
        })
    }

    /// Parse embedded manifests with the same schema and duplicate checks as files.
    pub fn from_sources(sources: &[impl AsRef<str>]) -> Result<Self> {
        let mut data = RegistryData::default();
        for raw in sources {
            Self::insert_source(&mut data, raw.as_ref(), None)
                .context("invalid embedded manifest")?;
        }
        Ok(Self {
            data: Arc::new(data),
        })
    }

    /// Create a registry scope for manifests generated from one declaring
    /// package manifest. Canonical applet names in the scope can shadow
    /// ambient catalog entries; phrase collisions remain errors.
    pub fn with_scoped_manifest_sources(&self, sources: &[String], origin: &Path) -> Result<Self> {
        if sources.is_empty() {
            return Ok(self.clone());
        }
        let origin = fs::canonicalize(origin).with_context(|| {
            format!("canonicalizing scoped manifest origin {}", origin.display())
        })?;
        let base_len = self.data.manifests.len();
        let mut manifests = self.data.manifests.to_parsed()?;
        let mut names = (*self.data.names).clone();
        if let Manifests::Bundled { index, .. } = self.data.manifests.as_ref() {
            for (key, position) in index.names() {
                names.insert((*key).to_owned(), usize::from(*position));
            }
        }
        let mut data = RegistryData {
            manifests: Arc::new(Manifests::default()),
            names: Arc::new(names),
            module_root: self.data.module_root.clone(),
            scope_identity: None,
            host_state: Arc::clone(&self.data.host_state),
        };
        let mut scoped_keys = BTreeSet::new();
        for source in sources {
            let mut manifest = parse_manifest(source)?;
            if manifest.execution_backend() == AppletBackend::ProgramModule
                && data.module_root.is_none()
            {
                bail!("program_module manifests require a registry module root");
            }
            manifest.origin = Some(origin.clone());
            let canonical_name = manifest.name.clone();
            let keys = std::iter::once(manifest.name.clone())
                .chain(manifest.phrases.iter().cloned())
                .collect::<Vec<_>>();
            for key in &keys {
                if !scoped_keys.insert(key.clone()) {
                    bail!("duplicate scoped manifest name or phrase {key:?}");
                }
                if key != &canonical_name && data.names.contains_key(key) {
                    bail!(
                        "scoped manifest phrase {key:?} conflicts with an existing name or phrase"
                    );
                }
            }
            let index = manifests.len();
            for key in keys {
                Arc::make_mut(&mut data.names).insert(key, index);
            }
            manifests.push(manifest);
        }
        data.scope_identity = Some(scoped_registry_identity(
            &manifests[..base_len],
            self.data.module_root.as_deref(),
            self.data.scope_identity.as_deref(),
            &origin,
            sources,
        )?);
        data.manifests = Arc::new(Manifests::Parsed(manifests));
        Ok(Self {
            data: Arc::new(data),
        })
    }

    /// A registry over the embedded bundled index, with origins under `root`. Each
    /// manifest is parsed on first use. Clones share the registry's parsed cells, and
    /// each root gets its own registry.
    pub fn from_bundled(root: &Path) -> Result<Self> {
        let index = bundled_manifests::index()?;
        let root = fs::canonicalize(root)
            .with_context(|| format!("canonicalizing manifest root {}", root.display()))?;
        let cells = (0..index.entries().len())
            .map(|_| OnceLock::new())
            .collect();
        let verified = (0..index.entries().len())
            .map(|_| OnceLock::new())
            .collect();
        Ok(Self {
            data: Arc::new(RegistryData {
                manifests: Arc::new(Manifests::Bundled {
                    index,
                    root: root.clone(),
                    cells,
                    verified,
                }),
                names: Arc::new(BTreeMap::new()),
                module_root: Some(root),
                ..RegistryData::default()
            }),
        })
    }

    /// Load the default manifest directory. When it holds the bundled file names (and,
    /// with [`ManifestCheck::Full`], their sizes), the bundled index serves it; any other
    /// directory takes the full load. A missing directory is an empty registry.
    pub fn load_default(path: &Path, check: ManifestCheck) -> Result<Self> {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                let index = bundled_manifests::index()?;
                if !matches!(bundled_manifests::directory_matches(path, index), Ok(true)) {
                    return Self::load(path);
                }
                if check == ManifestCheck::Full
                    && !index
                        .entries()
                        .iter()
                        .all(|entry| entry_matches(path, entry))
                {
                    return Self::load(path);
                }
                Self::from_bundled(path)
            }
            Ok(_) => Self::load(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("reading manifest path {}", path.display()))
            }
        }
    }

    fn insert_source(data: &mut RegistryData, raw: &str, origin: Option<PathBuf>) -> Result<()> {
        let mut manifest = parse_manifest(raw)?;
        if manifest.execution_backend() == AppletBackend::ProgramModule
            && (origin.is_none() || data.module_root.is_none())
        {
            bail!("program_module manifests require a file-loaded registry with a module root");
        }
        manifest.origin = origin;
        let Manifests::Parsed(list) = Arc::make_mut(&mut data.manifests) else {
            bail!("internal error: a bundled registry takes no new manifests");
        };
        let index = list.len();
        for key in std::iter::once(&manifest.name).chain(manifest.phrases.iter()) {
            if let Some(previous) = Arc::make_mut(&mut data.names).insert(key.clone(), index) {
                let previous_name = &list[previous].name;
                bail!(
                    "duplicate manifest name or phrase {key:?} (used by {previous_name:?} and {:?})",
                    manifest.name
                );
            }
        }
        list.push(manifest);
        Ok(())
    }

    pub fn resolve(&self, name_or_phrase: &str) -> Result<&Manifest> {
        let index = match self.data.manifests.as_ref() {
            Manifests::Bundled { index, .. } => index.lookup(name_or_phrase),
            Manifests::Parsed(_) => self.data.names.get(name_or_phrase).copied(),
        }
        .ok_or_else(|| anyhow!("unknown applet {name_or_phrase:?}"))?;
        self.data.manifests.get(index)
    }

    /// Every manifest in resolution order. On a bundled registry this parses any entry
    /// not yet used, so the first call parses the whole set.
    pub fn manifests(&self) -> Result<Vec<&Manifest>> {
        self.data.manifests.all()
    }

    /// Canonical applet names in resolution order, without parsing any manifest. A
    /// bundled registry reads them from its index.
    pub fn applet_names(&self) -> Vec<&str> {
        match self.data.manifests.as_ref() {
            Manifests::Parsed(list) => list.iter().map(|manifest| manifest.name.as_str()).collect(),
            Manifests::Bundled { index, .. } => index
                .entries()
                .iter()
                .map(bundled_manifests::BundledEntry::name)
                .collect(),
        }
    }

    /// Canonical filesystem root used to confine file-loaded program modules.
    /// Embedded manifests do not carry filesystem authority.
    pub fn module_root(&self) -> Option<&Path> {
        self.data.module_root.as_deref()
    }

    /// Stable identity for this generated applet scope, including its parent
    /// catalog, source manifests, origin, and module root.
    pub fn scope_identity(&self) -> Option<&str> {
        self.data.scope_identity.as_deref()
    }

    /// An independent registry over the same manifests, with a fresh host-state map. The
    /// manifests are copied, so host state set on the copy (a capability policy, for example)
    /// is visible neither to this registry nor to any other copy.
    pub fn detached(&self) -> Self {
        Self {
            data: Arc::new(RegistryData {
                manifests: Arc::clone(&self.data.manifests),
                names: Arc::clone(&self.data.names),
                module_root: self.data.module_root.clone(),
                scope_identity: self.data.scope_identity.clone(),
                host_state: Arc::new(Mutex::new(HashMap::new())),
            }),
        }
    }

    /// Access a host-owned value scoped to this registry and all of its clones.
    /// The state is dropped when the last registry clone is released.
    pub fn host_state<T>(&self) -> Arc<T>
    where
        T: Default + Any + Send + Sync + 'static,
    {
        let mut states = self
            .data
            .host_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = states.get(&TypeId::of::<T>()) {
            if let Ok(state) = Arc::clone(state).downcast::<T>() {
                return state;
            }
        }
        let state = Arc::new(T::default());
        states.insert(TypeId::of::<T>(), state.clone());
        state
    }

    /// Install or replace a host-owned value before execution. Registry clones
    /// see the same immutable configuration through [`Registry::host_state`].
    pub fn set_host_state<T>(&self, value: T) -> Arc<T>
    where
        T: Any + Send + Sync + 'static,
    {
        let state = Arc::new(value);
        self.data
            .host_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(TypeId::of::<T>(), state.clone());
        state
    }
}

fn scoped_registry_identity(
    catalog: &[Manifest],
    module_root: Option<&Path>,
    parent_scope: Option<&str>,
    origin: &Path,
    sources: &[String],
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"execute-manifest-scope-v1\0");
    if let Some(module_root) = module_root {
        update_identity_component(&mut hasher, module_root.to_string_lossy().as_bytes());
    } else {
        update_identity_component(&mut hasher, &[]);
    }
    update_identity_component(&mut hasher, parent_scope.unwrap_or_default().as_bytes());
    update_identity_component(&mut hasher, origin.to_string_lossy().as_bytes());
    for manifest in catalog {
        update_identity_component(&mut hasher, &serde_json::to_vec(manifest)?);
    }
    let mut sorted_sources = sources.iter().map(String::as_str).collect::<Vec<_>>();
    sorted_sources.sort_unstable();
    for source in sorted_sources {
        update_identity_component(&mut hasher, source.as_bytes());
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn update_identity_component(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

impl Manifest {
    /// The parsed, versioned execution contract, when one was declared.
    pub fn execution_contract(&self) -> Option<&AppletContract> {
        self.contract.as_ref()
    }

    /// The backend selected by a versioned contract. Legacy applets remain
    /// native commands; runtime builtins are still exposed separately.
    pub fn execution_backend(&self) -> AppletBackend {
        self.contract
            .as_ref()
            .map_or(AppletBackend::Command, |contract| contract.backend)
    }

    pub fn stdio(&self) -> Option<&StdioContract> {
        self.stdio.as_ref()
    }

    pub fn environment(&self) -> Option<&AppletEnvironment> {
        self.contract
            .as_ref()
            .and_then(|contract| contract.environment.as_ref())
    }

    pub fn origin(&self) -> Option<&Path> {
        self.origin.as_deref()
    }

    pub fn cache_dependencies(&self) -> &[String] {
        &self.cache_dependencies
    }

    /// Validate the applet parameter interface against a finite module export.
    /// The tuple order is `(name, type, required)`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn validate_program_module_inputs(
        &self,
        inputs: &[(String, InputType, bool)],
    ) -> Result<()> {
        if inputs.len() != self.parameters.len() {
            bail!(
                "program module declares {} inputs but applet manifest declares {} parameters",
                inputs.len(),
                self.parameters.len()
            );
        }
        let mut seen = BTreeSet::new();
        for (name, input_type, required) in inputs {
            if !seen.insert(name) {
                bail!("program module has duplicate input {name:?}");
            }
            let Some(spec) = self.parameters.get(name) else {
                bail!("program module input {name:?} is not an applet parameter");
            };
            let expected = parameter_input_kind(spec)?;
            if expected != *input_type {
                bail!(
                    "program module input {name:?} has type {input_type}, applet parameter expects {}",
                    spec.kind
                );
            }
            if spec.required != *required {
                bail!(
                    "program module input {name:?} required flag does not match applet parameter"
                );
            }
        }
        for name in self.parameters.keys() {
            if !seen.contains(name) {
                bail!("program module does not declare applet parameter {name:?}");
            }
        }
        Ok(())
    }

    /// Named typed input/output/event metadata for graph-capable consumers.
    pub fn ports(&self) -> Option<&AppletPorts> {
        self.contract
            .as_ref()
            .and_then(|contract| contract.ports.as_ref())
    }

    pub fn result_envelope(&self) -> bool {
        self.contract
            .as_ref()
            .is_some_and(|contract| contract.result_envelope)
    }

    /// Output type for binding validation. Raw bytes have no typed binding;
    /// paths intentionally use existing text bindings while retaining their
    /// distinct schema metadata in the applet contract.
    pub fn output_kind(&self) -> Option<InputType> {
        self.contract.as_ref().and_then(AppletContract::output_kind)
    }

    /// Report schema and framing mismatches for a native stdout-to-stdin pipe.
    /// Native stages exchange bytes directly, so semantic assignability alone
    /// is not enough: structured inputs must agree on explicit framing. A
    /// bytes input intentionally accepts any producer's stdout bytes.
    pub(crate) fn native_pipe_compatibility_issues(&self, consumer: &Manifest) -> Vec<String> {
        let Some(producer_contract) = self.execution_contract() else {
            return Vec::new();
        };
        let Some(input) = consumer.ports().and_then(|ports| ports.inputs.get("stdin")) else {
            return Vec::new();
        };
        if input.schema.kind == OutputType::Bytes {
            return Vec::new();
        };

        let (output_schema, output_encoding) = producer_contract.output.as_ref().map_or_else(
            || (leaf_value_schema(OutputType::Bytes), OutputEncoding::Raw),
            |output| (output_value_schema(output), output.encoding),
        );
        let mut issues = Vec::new();
        if producer_contract
            .output
            .as_ref()
            .is_some_and(|output| output.trim_final_lf)
        {
            issues.push(
                "trim_final_lf applies only to typed output decoding; native pipes preserve the final LF".into(),
            );
        }
        if !output_schema.is_assignable_to(&input.schema) {
            issues.push(format!(
                "stdout schema is {}, expected {}",
                output_type_name(output_schema.kind),
                output_type_name(input.schema.kind)
            ));
        }
        match input.encoding {
            Some(input_encoding) if output_encoding != input_encoding => issues.push(format!(
                "stdout encoding is {output_encoding:?}, expected stdin encoding {input_encoding:?}"
            )),
            None => issues.push("stdin encoding is not declared".into()),
            _ => {}
        }
        issues
    }

    pub fn success_status(&self) -> Option<i32> {
        self.contract
            .as_ref()
            .map_or(Some(0), AppletContract::success_status)
    }

    pub fn outcome_for(&self, status: i32) -> Outcome {
        self.contract.as_ref().map_or_else(
            || Outcome {
                status,
                name: if status == 0 {
                    "success".into()
                } else {
                    format!("exit_{status}")
                },
                kind: if status == 0 {
                    OutcomeKind::Success
                } else {
                    OutcomeKind::Failure
                },
                effect_certainty: if status == 0 {
                    EffectCertainty::KnownResponse
                } else {
                    EffectCertainty::Uncertain
                },
            },
            |contract| contract.outcome_for(status),
        )
    }

    /// Whether results of this in-process applet are raw UTF-8 text or a path.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn in_process_text_output(&self) -> bool {
        self.contract
            .as_ref()
            .is_some_and(|contract| contract.in_process_text_output())
    }

    /// Decode output only at the explicit contract boundary. Legacy output is
    /// always preserved as bytes; status acceptance is a separate runtime
    /// decision. For a typed contract, non-success is an error unless a
    /// declared result envelope preserves it as structured outcome data.
    pub fn decode_output(&self, bytes: &[u8], status: i32) -> Result<AppletOutput> {
        if let Some(contract) = &self.contract {
            contract.decode_output(bytes, status)
        } else {
            Ok(AppletOutput::Bytes(bytes.to_vec()))
        }
    }

    #[cfg(any(not(target_arch = "wasm32"), test))]
    pub(crate) fn has_json_encoded_output_schema(&self) -> bool {
        self.contract
            .as_ref()
            .and_then(|contract| contract.output.as_ref())
            .is_some_and(|schema| schema.encoding == OutputEncoding::Json)
    }

    #[cfg(any(not(target_arch = "wasm32"), test))]
    pub(crate) fn decode_json_output_value(
        &self,
        value: serde_json::Value,
        bytes: &[u8],
        status: i32,
    ) -> Result<AppletOutput> {
        self.contract
            .as_ref()
            .context("predecoded JSON output requires an execution contract")?
            .decode_json_output_value(value, bytes, status)
    }

    /// The gated build feature this applet needs. Remote-backed applets need `remote`
    /// regardless of what they declare; any other applet declares its own.
    pub fn required_build_feature(&self) -> Option<&str> {
        if self.execution_backend() == AppletBackend::Remote {
            return Some("remote");
        }
        self.build_feature.as_deref()
    }

    /// Whether this build includes the applet's gated feature. An applet with no
    /// requirement is always available.
    pub fn build_available(&self) -> bool {
        self.required_build_feature()
            .is_none_or(build_feature_enabled)
    }

    /// The refusal for an applet this build cannot run, or `None` when it can.
    pub fn build_unavailable_message(&self) -> Option<String> {
        let feature = self.required_build_feature()?;
        if build_feature_enabled(feature) {
            return None;
        }
        let capability = match feature {
            "remote" => "remote backend",
            "desktop" => "desktop renderer",
            _ => "gated feature",
        };
        Some(format!(
            "applet {} requires the full build: this build has no {capability}; use the full build",
            self.name
        ))
    }

    /// Describe the validated callable interface without executing or locating
    /// its binary. This is deliberately separate from Serialize: checkpoint
    /// identities retain their existing representation.
    pub fn contract(&self) -> serde_json::Value {
        let properties = self
            .parameters
            .iter()
            .map(|(name, spec)| {
                let mut schema = parameter_schema(spec.kind, spec.fields.as_ref());
                if let Some(items) = spec.item_type {
                    schema["items"] = parameter_schema(items, None);
                }
                if let Some(minimum) = spec.min_items {
                    schema["minItems"] = minimum.into();
                }
                (name.clone(), schema)
            })
            .collect::<serde_json::Map<_, _>>();
        let required = self
            .parameters
            .iter()
            .filter_map(|(name, spec)| spec.required.then_some(name))
            .collect::<Vec<_>>();
        let mut projection = serde_json::json!({
            "name": self.name,
            "phrases": self.phrases,
            "backend": if self.builtin.is_some() { "builtin" } else { "native_process" },
            "binary": self.binary,
            "builtin": self.builtin,
            "parameters_schema": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object", "properties": properties,
                "required": required, "additionalProperties": false
            },
            "argv_template": self.arguments,
            "io": {
                "stdin": if self.accepts_stdin { "bytes" } else { "not_accepted" },
                "stdout": if self.builtin.is_some() { "empty" } else { "bytes" },
                "output_schema": null
            },
            "execution": {
                "shell_expansion": false,
                "availability": if self.builtin.is_some() { "runtime_builtin" } else if self.build_available() { "not_checked" } else { "requires_full_build" },
                "sandbox": "not_enforced",
                "cacheable": self.cacheable,
                "uncertain_on_failure": self.uncertain_on_failure,
                "effect": if self.builtin.is_some() { "working_directory" } else { "native_command" }
            }
        });
        if let Some(feature) = self.required_build_feature() {
            projection["execution"]["build_feature"] = feature.into();
        }
        if let Some(contract) = &self.contract {
            projection["contract_version"] = contract.version.into();
            projection["io"]["output_schema"] = contract
                .output
                .as_ref()
                .map(OutputSchema::json_schema)
                .unwrap_or(serde_json::Value::Null);
            projection["execution"]["contract_backend"] =
                serde_json::to_value(contract.backend).expect("backend serializes");
            projection["execution"]["capabilities"] =
                serde_json::to_value(&contract.capabilities).expect("capabilities serialize");
            projection["execution"]["effects"] =
                serde_json::to_value(&contract.effects).expect("effects serialize");
            projection["execution"]["outcomes"] =
                serde_json::to_value(&contract.outcomes).expect("outcomes serialize");
            projection["execution"]["lifecycle"] =
                serde_json::to_value(&contract.lifecycle).expect("lifecycle serializes");
            projection["execution"]["result_envelope"] = contract.result_envelope.into();
            if let Some(schema) = contract.result_envelope_schema() {
                projection["execution"]["result_envelope_schema"] = schema.json_schema();
            }
            if let Some(program) = &contract.program {
                projection["execution"]["program_module"] =
                    serde_json::to_value(program).expect("program module serializes");
            }
            if let Some(remote) = &contract.remote {
                projection["execution"]["remote"] = serde_json::json!({
                    "configured": true,
                    "allow_discovery": remote.allow_discovery,
                    "pinned_descriptor": remote.descriptor_cache.is_some()
                });
            }
            if let Some(environment) = &contract.environment {
                projection["execution"]["environment"] = serde_json::json!({
                    "set": environment.set.keys().collect::<Vec<_>>(),
                    "unset": environment.unset.iter().collect::<Vec<_>>()
                });
            }
            if let Some(ports) = &contract.ports {
                projection["ports"] = serde_json::to_value(ports).expect("ports serialize");
            }
        }
        if !self.cache_dependencies.is_empty() {
            projection["execution"]["cache_dependencies"] =
                serde_json::to_value(&self.cache_dependencies)
                    .expect("cache dependencies serialize");
        }
        if let Some(stdio) = &self.stdio {
            projection["io"]["stdio"] = serde_json::to_value(stdio).expect("stdio serializes");
        }
        projection
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn checkpoint_identity(&self, call: &Invocation) -> Result<Vec<u8>> {
        if call.direct {
            // Widening accepted scalar argv does not change the command mapping
            // for existing calls. Keep the 0.7.0 direct-call identity; actual
            // values and new input signatures are fingerprinted independently.
            let mut legacy = self.clone();
            legacy
                .parameters
                .get_mut("arguments")
                .expect("direct argv schema")
                .item_type = Some(ParameterType::String);
            Ok(serde_json::to_vec(&legacy)?)
        } else {
            Ok(serde_json::to_vec(self)?)
        }
    }

    pub fn name_for<'a>(&'a self, invocation: &'a Invocation) -> &'a str {
        if invocation.direct {
            &invocation.operation
        } else {
            &self.name
        }
    }

    pub fn binary_for<'a>(&'a self, invocation: &'a Invocation) -> &'a str {
        if invocation.direct {
            &invocation.operation
        } else {
            &self.binary
        }
    }

    /// Check names, required parameters, and statically known types. Named
    /// results contain text; their string contents are checked after resolution.
    pub fn validate(&self, invocation: &Invocation) -> Result<()> {
        self.validate_with_types(invocation, &BTreeMap::new())
    }

    pub(crate) fn validate_with_types(
        &self,
        invocation: &Invocation,
        bindings: &BTreeMap<String, InputType>,
    ) -> Result<()> {
        if invocation.binding.is_some()
            && self
                .contract
                .as_ref()
                .is_some_and(|contract| contract.output_kind().is_none())
        {
            bail!("raw bytes output cannot be bound to a typed applet value");
        }
        for name in invocation.parameters.keys() {
            if !self.parameters.contains_key(name) {
                bail!("applet {:?} has no parameter {name:?}", self.name);
            }
        }
        for (name, spec) in &self.parameters {
            if spec.required && !invocation.parameters.contains_key(name) {
                bail!("applet {:?} requires parameter {name:?}", self.name);
            }
        }
        // Literal values bound to a prefixed argument are checked now, so a
        // refused address fails at check time. Resolved values are checked again
        // when the argument is built, before any process starts.
        let mut prefixed = Vec::new();
        prefixed_parameter_names(&self.arguments, &mut prefixed);
        for name in prefixed {
            if let Some(Value::String(path)) = invocation.parameters.get(name) {
                check_prefixed_path(name, path)?;
            }
        }
        for (name, value) in &invocation.parameters {
            let spec = &self.parameters[name];
            if let Value::Reference(reference) = value {
                let kind = bindings.get(reference).copied().unwrap_or(InputType::Text);
                if !parameter_accepts_reference(spec.kind, spec.item_type, kind) {
                    bail!(
                        "parameter {name:?} expects {}, but reference {reference:?} contains {kind}; named results contain text",
                        spec.kind,
                    );
                }
                if self
                    .ports()
                    .and_then(|ports| ports.inputs.get(name))
                    .is_some_and(|port| !schema_accepts_reference(&port.schema, kind))
                {
                    bail!(
                        "parameter {name:?} contract port does not accept reference {reference:?} of type {kind}"
                    );
                }
                continue;
            }
            if let Value::List(items) = value {
                for item in items {
                    if let Value::Reference(reference) = item {
                        let kind = bindings.get(reference).copied().unwrap_or(InputType::Text);
                        if !list_item_accepts_reference(spec.item_type, kind) {
                            bail!(
                                "parameter {name:?} requires {} list items, but reference {reference:?} contains {kind}",
                                spec.item_type.expect("list parameter has an item type")
                            );
                        }
                    }
                }
            }
            validate_value(name, value, spec, true, bindings)?;
            if let Some(port) = self.ports().and_then(|ports| ports.inputs.get(name)) {
                if contains_parameter_reference(value) {
                    validate_port_value_with_references(&port.schema, value, bindings)
                        .with_context(|| format!("validating input port {name:?}"))?;
                } else {
                    port.schema
                        .validate_value(value)
                        .with_context(|| format!("validating input port {name:?}"))?;
                }
            }
        }
        Ok(())
    }

    /// Validate resolved values and produce the exact argv vector for the child.
    pub fn build_arguments(&self, values: &BTreeMap<String, Value>) -> Result<Vec<String>> {
        let values = self.home_expanded_values(values)?;
        self.validate_argument_values(values.as_ref())?;
        let mut argv = Vec::with_capacity(self.arguments.len());
        for node in &self.arguments {
            append_arguments(node, &values, &mut argv)?;
        }
        Ok(argv)
    }

    /// Return the supplied, already-resolved parameters as a JSON object for
    /// in-process applet dispatch. Optional parameters remain omitted. The values may
    /// be borrowed (a binding passed by reference is validated and converted in place,
    /// never cloned first); they are copied only when a path needs `~` expanded.
    pub fn parameters_json<V: Borrow<Value>>(
        &self,
        values: &BTreeMap<String, V>,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        self.parameters_json_skipping(values, None)
    }

    /// Validate every supplied parameter while omitting one value from the converted
    /// applet argument map. This is used when an in-process applet can consume a typed
    /// binding directly, avoiding a full JSON tree copy for that parameter.
    pub(crate) fn parameters_json_skipping<V: Borrow<Value>>(
        &self,
        values: &BTreeMap<String, V>,
        skipped: Option<&str>,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        let needs_home = values.iter().any(|(name, value)| {
            self.is_path_parameter(name)
                && matches!(value.borrow(), Value::String(text) if starts_with_home(text))
        });
        if needs_home {
            let owned = values
                .iter()
                .map(|(name, value)| (name.clone(), value.borrow().clone()))
                .collect::<BTreeMap<_, _>>();
            let expanded = self.home_expanded_values(&owned)?;
            return self.converted_parameters_skipping(expanded.as_ref(), skipped);
        }
        self.converted_parameters_skipping(values, skipped)
    }

    fn converted_parameters_skipping<V: Borrow<Value>>(
        &self,
        values: &BTreeMap<String, V>,
        skipped: Option<&str>,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        self.validate_argument_values_skipping_json_port(values, skipped)?;
        values
            .iter()
            .filter(|(name, _)| skipped != Some(name.as_str()))
            .map(|(name, value)| Ok((name.clone(), crate::structured::to_json(value.borrow())?)))
            .collect()
    }

    /// Whether the manifest declares this parameter with the `path` type. Only such
    /// parameters have a leading `~/` expanded to the home directory; string and
    /// list parameters, and literal argv text, keep `~` as written.
    pub fn is_path_parameter(&self, name: &str) -> bool {
        self.parameters
            .get(name)
            .is_some_and(|spec| spec.kind == ParameterType::Path)
    }

    /// The supplied values with a leading `~` expanded in path-typed parameters.
    /// Borrowed when nothing needs expanding, so ordinary calls allocate nothing.
    fn home_expanded_values<'a>(
        &self,
        values: &'a BTreeMap<String, Value>,
    ) -> Result<Cow<'a, BTreeMap<String, Value>>> {
        let needs_home = |name: &str, value: &Value| {
            self.is_path_parameter(name)
                && matches!(value, Value::String(text) if starts_with_home(text))
        };
        if !values.iter().any(|(name, value)| needs_home(name, value)) {
            return Ok(Cow::Borrowed(values));
        }
        let mut owned = values.clone();
        for (name, value) in owned.iter_mut() {
            if !self.is_path_parameter(name) {
                continue;
            }
            if let Value::String(text) = value {
                if starts_with_home(text) {
                    let expanded = crate::home::expand_home_path(text)
                        .with_context(|| format!("parameter {name:?}"))?;
                    *text = expanded.into_os_string().into_string().map_err(|_| {
                        anyhow!("parameter {name:?}: the home directory is not UTF-8")
                    })?;
                }
            }
        }
        Ok(Cow::Owned(owned))
    }

    /// Build argv for immediate Command construction without cloning borrowed text.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn build_arguments_borrowed<'a>(
        &'a self,
        values: &'a BTreeMap<String, Value>,
    ) -> Result<Vec<Cow<'a, str>>> {
        let values = match self.home_expanded_values(values)? {
            Cow::Borrowed(values) => values,
            // An expanded path is an owned string, so the owned builder is used.
            Cow::Owned(owned) => {
                return Ok(self
                    .build_arguments(&owned)?
                    .into_iter()
                    .map(Cow::Owned)
                    .collect());
            }
        };
        self.validate_argument_values(values)?;
        let mut argv = Vec::with_capacity(self.arguments.len());
        for node in &self.arguments {
            append_arguments_borrowed(node, values, &mut argv)?;
        }
        Ok(argv)
    }

    fn validate_argument_values<V: Borrow<Value>>(
        &self,
        values: &BTreeMap<String, V>,
    ) -> Result<()> {
        self.validate_argument_values_skipping_json_port(values, None)
    }

    fn validate_argument_values_skipping_json_port<V: Borrow<Value>>(
        &self,
        values: &BTreeMap<String, V>,
        skipped_json_port: Option<&str>,
    ) -> Result<()> {
        // Both maps are ordered. Merge their keys to find the first unknown
        // supplied name in linear time while preserving the old error order.
        let supplied_names = values.keys();
        let mut parameter_names = self.parameters.keys().peekable();
        for name in supplied_names {
            while parameter_names
                .peek()
                .is_some_and(|parameter| parameter.as_str() < name.as_str())
            {
                parameter_names.next();
            }
            if parameter_names
                .peek()
                .is_none_or(|parameter| parameter.as_str() != name.as_str())
            {
                bail!("applet {:?} has no parameter {name:?}", self.name);
            }
        }

        let mut supplied = values.iter().peekable();
        for (name, spec) in &self.parameters {
            while supplied
                .peek()
                .is_some_and(|(supplied_name, _)| supplied_name.as_str() < name.as_str())
            {
                supplied.next();
            }
            let value = match supplied.peek() {
                Some((supplied_name, value)) if supplied_name.as_str() == name.as_str() => {
                    let value: &Value = (*value).borrow();
                    supplied.next();
                    Some(value)
                }
                _ => None,
            };
            match value {
                None if spec.required => {
                    bail!("applet {:?} requires parameter {name:?}", self.name)
                }
                None => {}
                Some(value) => {
                    validate_value(name, value, spec, false, &BTreeMap::new())?;
                    // Resolved values are checked explicitly, so the first-token
                    // rule does not depend on the static path's deferral.
                    if let Value::List(items) = value {
                        check_first_token(name, spec, items)?;
                        check_option_items(name, spec, items, false)?;
                    }
                    if let Some(port) = self.ports().and_then(|ports| ports.inputs.get(name)) {
                        if skipped_json_port == Some(name.as_str())
                            && port.schema.kind == crate::contracts::OutputType::Json
                            && !contains_parameter_reference(value)
                        {
                            // A reference-free AST JSON value is already a valid JSON
                            // representation. Keep schema validation, but avoid creating
                            // a serde_json tree solely to validate a `json` port.
                            port.schema.validate()?;
                        } else {
                            port.schema
                                .validate_value(value)
                                .with_context(|| format!("validating input port {name:?}"))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn parameter_schema(
    kind: ParameterType,
    fields: Option<&BTreeMap<String, ParameterFieldSpec>>,
) -> serde_json::Value {
    match kind {
        ParameterType::String => serde_json::json!({"type": "string", "pattern": "^[^\\u0000]*$"}),
        ParameterType::Path => {
            serde_json::json!({"type": "string", "format":"path", "minLength":1, "pattern": "^[^\\u0000]+$"})
        }
        ParameterType::Integer => serde_json::json!({
            "type": "integer", "minimum": i64::MIN, "maximum": i64::MAX
        }),
        ParameterType::Boolean => serde_json::json!({"type": "boolean"}),
        ParameterType::List => serde_json::json!({"type": "array"}),
        ParameterType::Json => serde_json::json!({}),
        ParameterType::Record => {
            let fields = fields.expect("record parameter fields validated");
            let properties = fields
                .iter()
                .map(|(name, field)| (name.clone(), parameter_field_schema(field)))
                .collect::<serde_json::Map<_, _>>();
            let required = fields
                .iter()
                .filter_map(|(name, field)| field.required.then_some(name))
                .collect::<Vec<_>>();
            serde_json::json!({
                "type":"object", "properties":properties,
                "required":required, "additionalProperties":false
            })
        }
        ParameterType::Scalar => serde_json::json!({
            "anyOf": [parameter_schema(ParameterType::String, None),
                parameter_schema(ParameterType::Integer, None), parameter_schema(ParameterType::Boolean, None)]
        }),
    }
}

fn parameter_field_schema(field: &ParameterFieldSpec) -> serde_json::Value {
    parameter_schema(field.kind, field.fields.as_ref())
}

/// Check whether a typed reference can satisfy an applet parameter. JSON is
/// deliberately deferred: its runtime value may satisfy any parameter type,
/// but a partial JSON input is not itself proof of that value's shape. Runtime
/// validation checks the resolved value before invocation.
fn parameter_accepts_reference(
    expected: ParameterType,
    item_type: Option<ParameterType>,
    actual: InputType,
) -> bool {
    if actual == InputType::Json {
        return true;
    }
    matches!(
        (expected, actual),
        (ParameterType::String | ParameterType::Path, InputType::Text)
            | (ParameterType::Integer, InputType::Integer)
            | (ParameterType::Boolean, InputType::Boolean)
            | (ParameterType::Json, _)
            | (ParameterType::Record, InputType::Json)
    ) || (expected == ParameterType::List
        && actual == InputType::TextList
        && matches!(
            item_type,
            Some(ParameterType::String | ParameterType::Path | ParameterType::Scalar)
        ))
}

/// A reference used as one list item must resolve to a single scalar of the
/// declared item type. Whole-list JSON references are handled by
/// `parameter_accepts_reference`; list-valued references inside a list are
/// intentionally unsupported.
fn list_item_accepts_reference(item_type: Option<ParameterType>, actual: InputType) -> bool {
    if actual == InputType::Json {
        // A JSON reference is an aggregate at this boundary. Whole-list JSON
        // references may defer their shape check, but a scalar list item must
        // have a statically scalar input type.
        return false;
    }
    matches!(
        (item_type, actual),
        (
            Some(ParameterType::String | ParameterType::Path),
            InputType::Text
        ) | (Some(ParameterType::Integer), InputType::Integer)
            | (Some(ParameterType::Boolean), InputType::Boolean)
            | (
                Some(ParameterType::Scalar),
                InputType::Text | InputType::Integer | InputType::Boolean
            )
    )
}

fn validate_value(
    name: &str,
    value: &Value,
    spec: &ParameterSpec,
    allow_reference_items: bool,
    bindings: &BTreeMap<String, InputType>,
) -> Result<()> {
    let valid = matches!(
        (spec.kind, value),
        (
            ParameterType::String | ParameterType::Path,
            Value::String(_)
        ) | (ParameterType::Integer, Value::Integer(_))
            | (ParameterType::Boolean, Value::Bool(_))
            | (ParameterType::List, Value::List(_))
            | (
                ParameterType::Json,
                Value::String(_)
                    | Value::Integer(_)
                    | Value::Number(_)
                    | Value::Bool(_)
                    | Value::List(_)
                    | Value::Record(_)
                    | Value::Null
            )
            | (ParameterType::Record, Value::Record(_))
    );
    if !valid {
        bail!(
            "parameter {name:?} expects {}, got {}",
            spec.kind,
            value_kind(value)
        );
    }
    if let Value::String(string) = value {
        if string.contains('\0') {
            bail!("parameter {name:?} contains a NUL byte, which cannot be passed in argv");
        }
        if spec.kind == ParameterType::Path && string.is_empty() {
            bail!("path parameter {name:?} must be nonempty");
        }
    }
    if let (ParameterType::Record, Value::Record(values)) = (spec.kind, value) {
        validate_record_parameter(
            name,
            values,
            spec.fields.as_ref().expect("record schema validated"),
            allow_reference_items,
            bindings,
        )?;
    }
    if let (ParameterType::List, Value::List(items)) = (spec.kind, value) {
        let item_type = spec.item_type.expect("list schema validated");
        let mut contains_nul = false;
        for item in items {
            if !(is_scalar_type(item, item_type)
                || allow_reference_items
                    && matches!(
                        item_type,
                        ParameterType::String
                            | ParameterType::Path
                            | ParameterType::Integer
                            | ParameterType::Boolean
                            | ParameterType::Scalar
                    )
                    && matches!(item, Value::Reference(_)))
            {
                bail!(
                    "parameter {name:?} contains an item that is not {}",
                    item_type
                );
            }
            contains_nul |= matches!(item, Value::String(string) if string.contains('\0'));
            if item_type == ParameterType::Path
                && matches!(item, Value::String(string) if string.is_empty())
            {
                bail!("parameter {name:?} contains an empty path item");
            }
        }
        if contains_nul {
            bail!("parameter {name:?} contains a NUL byte, which cannot be passed in argv");
        }
        if let Some(minimum) = spec.min_items {
            if items.len() < minimum {
                bail!("parameter {name:?} needs at least {minimum} item(s)");
            }
        }
        // A reference first item is deferred: validate_argument_values checks the
        // resolved list (with references disallowed) before any argv is built.
        if !matches!(items.first(), Some(Value::Reference(_))) {
            check_first_token(name, spec, items)?;
        }
        check_option_items(name, spec, items, allow_reference_items)?;
    }
    Ok(())
}

/// Enforce a list's first-token rules on the list's values. The whole first item
/// is compared, exactly and case-insensitively, against the allow or deny list.
/// There is no word extraction: a sub-word can hide a dispatcher, and a leading
/// `-` is an option to the tool, so a first item that is empty, whitespace-only,
/// or starts with `-` is refused whenever a rule applies. The check fails closed:
/// a missing first item (for an allow list), a non-text first item, or an
/// unresolved reference is an error, never a pass. Callers run it on resolved
/// values before any argument is rendered.
fn check_first_token(name: &str, spec: &ParameterSpec, items: &[Value]) -> Result<()> {
    if spec.allow_first_token.is_empty() && spec.deny_first_token.is_empty() {
        return Ok(());
    }
    match items.first() {
        None if !spec.allow_first_token.is_empty() => {
            bail!("parameter {name:?} requires a first item")
        }
        None => Ok(()),
        Some(Value::String(first)) => {
            if first.trim().is_empty() {
                bail!("parameter {name:?} first item must not be empty");
            }
            if first.trim_start().starts_with('-') {
                bail!(
                    "parameter {name:?} first item {first:?} starts with '-' and would be read as an option"
                );
            }
            if !spec.allow_first_token.is_empty()
                && !spec
                    .allow_first_token
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(first))
            {
                bail!(
                    "parameter {name:?} first item must be one of {:?}, got {first:?}",
                    spec.allow_first_token
                );
            }
            if spec
                .deny_first_token
                .iter()
                .any(|denied| denied.eq_ignore_ascii_case(first))
            {
                bail!("parameter {name:?} first item {first:?} is refused");
            }
            Ok(())
        }
        Some(other) => bail!(
            "parameter {name:?} first item must be resolved text when first-token rules apply, got {}",
            value_kind(other)
        ),
    }
}

/// Refuse list items that a tool would read as an option or as a command batch.
/// The rule applies to every item, not only the first, because a later
/// `--batch` or `;`-separated command still runs. A plain number may start with
/// `-` (for example a resize amount), but nothing else may. Separators and
/// control bytes are refused everywhere. `allow_references` is true for the
/// static check; at argument-build time every item must already be text.
fn check_option_items(
    name: &str,
    spec: &ParameterSpec,
    items: &[Value],
    allow_references: bool,
) -> Result<()> {
    if !spec.forbid_option_items {
        return Ok(());
    }
    for (index, item) in items.iter().enumerate() {
        let text = match item {
            Value::String(text) => text,
            Value::Reference(_) if allow_references => continue,
            other => bail!(
                "parameter {name:?} item {index} must be resolved text when option rules apply, got {}",
                value_kind(other)
            ),
        };
        if text.contains([';', '\n', '\r', '\0']) {
            bail!("parameter {name:?} item {index} contains a command separator or control byte");
        }
        let trimmed = text.trim();
        if trimmed.starts_with('-') && !is_plain_number(trimmed) {
            bail!(
                "parameter {name:?} item {index} {text:?} would be read as an option; only plain numbers may start with '-'"
            );
        }
    }
    Ok(())
}

/// `-?[0-9]+(\.[0-9]+)?%?`, the number shapes a resize or move amount uses.
fn is_plain_number(text: &str) -> bool {
    let text = text.strip_suffix('%').unwrap_or(text);
    let text = text.strip_prefix('-').unwrap_or(text);
    let mut parts = text.splitn(2, '.');
    let integer = parts.next().unwrap_or("");
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    digits(integer) && parts.next().is_none_or(digits)
}

fn validate_record_parameter(
    name: &str,
    values: &BTreeMap<String, Value>,
    fields: &BTreeMap<String, ParameterFieldSpec>,
    allow_references: bool,
    bindings: &BTreeMap<String, InputType>,
) -> Result<()> {
    for (field_name, field) in fields {
        match values.get(field_name) {
            None if field.required => {
                bail!("parameter {name:?} is missing required field {field_name:?}")
            }
            None => {}
            Some(value) => validate_parameter_field(
                &format!("{name}.{field_name}"),
                value,
                field,
                allow_references,
                bindings,
            )?,
        }
    }
    for field_name in values.keys() {
        if !fields.contains_key(field_name) {
            bail!("parameter {name:?} has undeclared field {field_name:?}");
        }
    }
    Ok(())
}

fn validate_parameter_field(
    name: &str,
    value: &Value,
    field: &ParameterFieldSpec,
    allow_references: bool,
    bindings: &BTreeMap<String, InputType>,
) -> Result<()> {
    if let Value::Reference(reference) = value {
        if allow_references {
            let kind = bindings.get(reference).copied().unwrap_or(InputType::Text);
            if !parameter_accepts_reference(field.kind, None, kind) {
                bail!(
                    "record field {name:?} expects {}, but reference {reference:?} contains {kind}",
                    field.kind
                );
            }
            return Ok(());
        }
    }
    let valid = match field.kind {
        ParameterType::String | ParameterType::Path => matches!(value, Value::String(_)),
        ParameterType::Integer => matches!(value, Value::Integer(_)),
        ParameterType::Boolean => matches!(value, Value::Bool(_)),
        ParameterType::Json => matches!(
            value,
            Value::String(_)
                | Value::Integer(_)
                | Value::Number(_)
                | Value::Bool(_)
                | Value::List(_)
                | Value::Record(_)
                | Value::Null
        ),
        ParameterType::Record => matches!(value, Value::Record(_)),
        ParameterType::List | ParameterType::Scalar => false,
    };
    if !valid {
        bail!(
            "record field {name:?} expects {}, got {}",
            field.kind,
            value_kind(value)
        );
    }
    if let Value::String(string) = value {
        if string.contains('\0') {
            bail!("record field {name:?} contains a NUL byte");
        }
        if field.kind == ParameterType::Path && string.is_empty() {
            bail!("path field {name:?} must be nonempty");
        }
    }
    if field.kind == ParameterType::Record {
        let Value::Record(values) = value else {
            unreachable!("type checked above")
        };
        validate_record_parameter(
            name,
            values,
            field
                .fields
                .as_ref()
                .expect("record field schema validated"),
            allow_references,
            bindings,
        )?;
    }
    Ok(())
}

fn is_scalar_type(value: &Value, expected: ParameterType) -> bool {
    matches!(
        (expected, value),
        (ParameterType::String, Value::String(_))
            | (ParameterType::Path, Value::String(_))
            | (ParameterType::Integer, Value::Integer(_))
            | (ParameterType::Boolean, Value::Bool(_))
            | (
                ParameterType::Scalar,
                Value::String(_) | Value::Integer(_) | Value::Bool(_)
            )
    )
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Bool(_) => "boolean",
        Value::List(_) => "list",
        Value::Reference(_) => "reference",
        Value::Record(_) | Value::Null | Value::Number(_) => "json",
    }
}

/// Render `prefix` followed by a validated path parameter as one argument.
fn prefixed_argument(
    prefix: &str,
    values: &BTreeMap<String, Value>,
    parameter: &str,
) -> Result<String> {
    let value = values
        .get(parameter)
        .ok_or_else(|| anyhow!("missing parameter {parameter:?}"))?;
    let Value::String(path) = value else {
        bail!("prefixed parameter {parameter:?} must be text");
    };
    check_prefixed_path(parameter, path)?;
    Ok(format!("{prefix}{path}"))
}

fn prefixed_parameter_names<'a>(nodes: &'a [ArgumentNode], out: &mut Vec<&'a str>) {
    for node in nodes {
        match node {
            ArgumentNode::PrefixedParameter { parameter, .. } => out.push(parameter),
            ArgumentNode::WhenPresent { append, .. } => prefixed_parameter_names(append, out),
            _ => {}
        }
    }
}

/// A prefixed value is appended to a tool's own address or mini-language, so it
/// must be a plain absolute path. Address syntax treats `,`, `:` and `!` as
/// separators and options, and whitespace or control bytes as delimiters, so
/// those are refused here, before any process starts. `..` components are
/// refused so the path cannot climb out of the directory it names.
fn check_prefixed_path(parameter: &str, path: &str) -> Result<()> {
    if path.len() < 2 || !path.starts_with('/') {
        bail!("parameter {parameter:?} must be an absolute path below /");
    }
    if path
        .chars()
        .any(|c| matches!(c, ',' | ':' | '!') || c.is_whitespace() || c.is_control())
    {
        bail!(
            "parameter {parameter:?} contains a character reserved by address syntax (, : ! whitespace or control)"
        );
    }
    if path.split('/').any(|part| part == "..") {
        bail!("parameter {parameter:?} must not contain .. components");
    }
    Ok(())
}

/// Whether a path value begins with the home shorthand: `~`, `~/`, or `~\` on Windows.
/// `~user` is not a home shorthand and is never expanded.
fn starts_with_home(text: &str) -> bool {
    text == "~" || text.starts_with("~/") || (cfg!(windows) && text.starts_with("~\\"))
}

fn append_arguments(
    node: &ArgumentNode,
    values: &BTreeMap<String, Value>,
    out: &mut Vec<String>,
) -> Result<()> {
    match node {
        ArgumentNode::Literal { literal } => out.push(literal.clone()),
        ArgumentNode::Parameter { parameter } => {
            let value = values
                .get(parameter)
                .ok_or_else(|| anyhow!("missing parameter {parameter:?}"))?;
            out.push(value_to_string(value)?);
        }
        ArgumentNode::Spread { spread } => {
            let value = values
                .get(spread)
                .ok_or_else(|| anyhow!("missing parameter {spread:?}"))?;
            let Value::List(items) = value else {
                bail!("parameter {spread:?} must be a list")
            };
            out.reserve(items.len());
            for item in items {
                out.push(value_to_string(item)?);
            }
        }
        ArgumentNode::JsonParameter { json_parameter } => {
            let value = values
                .get(json_parameter)
                .ok_or_else(|| anyhow!("missing parameter {json_parameter:?}"))?;
            out.push(json_value_to_string(value)?);
        }
        ArgumentNode::JsonArguments => out.push(json_arguments_to_string(values)?),
        ArgumentNode::PrefixedParameter { prefix, parameter } => {
            out.push(prefixed_argument(prefix, values, parameter)?);
        }
        ArgumentNode::WhenPresent {
            when_present,
            append,
        } => {
            if values.contains_key(when_present) {
                for child in append {
                    append_arguments(child, values, out)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn append_arguments_borrowed<'a>(
    node: &'a ArgumentNode,
    values: &'a BTreeMap<String, Value>,
    out: &mut Vec<Cow<'a, str>>,
) -> Result<()> {
    match node {
        ArgumentNode::Literal { literal } => out.push(Cow::Borrowed(literal)),
        ArgumentNode::Parameter { parameter } => {
            let value = values
                .get(parameter)
                .ok_or_else(|| anyhow!("missing parameter {parameter:?}"))?;
            out.push(value_to_cow(value)?);
        }
        ArgumentNode::Spread { spread } => {
            let value = values
                .get(spread)
                .ok_or_else(|| anyhow!("missing parameter {spread:?}"))?;
            let Value::List(items) = value else {
                bail!("parameter {spread:?} must be a list")
            };
            out.reserve(items.len());
            for item in items {
                out.push(value_to_cow(item)?);
            }
        }
        ArgumentNode::JsonParameter { json_parameter } => {
            let value = values
                .get(json_parameter)
                .ok_or_else(|| anyhow!("missing parameter {json_parameter:?}"))?;
            out.push(Cow::Owned(json_value_to_string(value)?));
        }
        ArgumentNode::JsonArguments => {
            out.push(Cow::Owned(json_arguments_to_string(values)?));
        }
        ArgumentNode::PrefixedParameter { prefix, parameter } => {
            out.push(Cow::Owned(prefixed_argument(prefix, values, parameter)?));
        }
        ArgumentNode::WhenPresent {
            when_present,
            append,
        } => {
            if values.contains_key(when_present) {
                for child in append {
                    append_arguments_borrowed(child, values, out)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn value_to_cow(value: &Value) -> Result<Cow<'_, str>> {
    match value {
        Value::String(value) => Ok(Cow::Borrowed(value)),
        Value::Integer(value) => Ok(Cow::Owned(value.to_string())),
        Value::Bool(value) => Ok(Cow::Borrowed(if *value { "true" } else { "false" })),
        Value::List(_) => bail!("cannot use a list as one argument"),
        Value::Reference(_) => bail!("unresolved reference cannot become an argument"),
        Value::Record(_) | Value::Null | Value::Number(_) => {
            bail!("select a typed scalar or encode JSON before passing an argument")
        }
    }
}

fn value_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Integer(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::List(_) => bail!("cannot use a list as one argument"),
        Value::Reference(_) => bail!("unresolved reference cannot become an argument"),
        Value::Record(_) | Value::Null | Value::Number(_) => {
            bail!("select a typed scalar or encode JSON before passing an argument")
        }
    }
}

fn json_value_to_string(value: &Value) -> Result<String> {
    let json = crate::structured::to_json(value)?;
    Ok(serde_json::to_string(&json)?)
}

fn json_arguments_to_string(values: &BTreeMap<String, Value>) -> Result<String> {
    let object = values
        .iter()
        .map(|(name, value)| Ok((name.clone(), crate::structured::to_json(value)?)))
        .collect::<Result<serde_json::Map<_, _>>>()?;
    Ok(serde_json::to_string(&serde_json::Value::Object(object))?)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    #[serde(default)]
    phrases: Vec<String>,
    binary: String,
    #[serde(default)]
    builtin: Option<Builtin>,
    #[serde(default)]
    accepts_stdin: bool,
    #[serde(default)]
    cacheable: bool,
    #[serde(default)]
    cache_dependencies: Vec<String>,
    #[serde(default)]
    uncertain_on_failure: bool,
    #[serde(default, deserialize_with = "crate::contracts::deserialize_present")]
    contract: Option<AppletContract>,
    #[serde(default)]
    build_feature: Option<String>,
    #[serde(default, deserialize_with = "crate::contracts::deserialize_present")]
    stdio: Option<StdioContract>,
    #[serde(default)]
    parameters: BTreeMap<String, ParameterSpec>,
    arguments: Vec<RawArgument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParameter {
    #[serde(rename = "type")]
    kind: ParameterType,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    items: Option<ParameterType>,
    #[serde(default)]
    min_items: Option<usize>,
    #[serde(default)]
    allow_first_token: Vec<String>,
    #[serde(default)]
    deny_first_token: Vec<String>,
    #[serde(default)]
    forbid_option_items: bool,
    #[serde(default)]
    fields: Option<BTreeMap<String, ParameterFieldSpec>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParameterField {
    #[serde(rename = "type")]
    kind: ParameterType,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    fields: Option<BTreeMap<String, ParameterFieldSpec>>,
}

#[derive(Debug)]
enum RawArgument {
    Literal {
        literal: String,
    },
    Parameter {
        parameter: String,
    },
    Spread {
        spread: String,
    },
    JsonParameter {
        json_parameter: String,
    },
    JsonArguments,
    PrefixedParameter {
        prefix: String,
        parameter: String,
    },
    WhenPresent {
        when_present: String,
        append: Vec<RawArgument>,
    },
}

impl<'de> Deserialize<'de> for RawArgument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Missing fields and explicit null have different meanings: all present
        // values must decode to their required type, including in mixed nodes.
        fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
            deserializer: D,
        ) -> Result<Option<T>, D::Error> {
            T::deserialize(deserializer).map(Some)
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(default, deserialize_with = "present")]
            literal: Option<String>,
            #[serde(default, deserialize_with = "present")]
            parameter: Option<String>,
            #[serde(default, deserialize_with = "present")]
            spread: Option<String>,
            #[serde(default, deserialize_with = "present")]
            json_parameter: Option<String>,
            #[serde(default, deserialize_with = "present")]
            json_arguments: Option<bool>,
            #[serde(default, deserialize_with = "present")]
            when_present: Option<String>,
            #[serde(default, deserialize_with = "present")]
            append: Option<Vec<RawArgument>>,
            #[serde(default, deserialize_with = "present")]
            prefix: Option<String>,
        }
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = RawArgument;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a manifest argument node")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields =
                    Fields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                if let Some(prefix) = fields.prefix.take() {
                    return match (
                        fields.parameter,
                        fields.literal,
                        fields.spread,
                        fields.json_parameter,
                        fields.json_arguments,
                        fields.when_present,
                        fields.append,
                    ) {
                        (Some(parameter), None, None, None, None, None, None) => {
                            Ok(RawArgument::PrefixedParameter { prefix, parameter })
                        }
                        _ => Err(serde::de::Error::custom(
                            "prefix requires exactly one parameter and no other argument fields",
                        )),
                    };
                }
                match (
                    fields.literal,
                    fields.parameter,
                    fields.spread,
                    fields.json_parameter,
                    fields.json_arguments,
                    fields.when_present,
                    fields.append,
                ) {
                    (Some(literal), None, None, None, None, None, None) => {
                        Ok(RawArgument::Literal { literal })
                    }
                    (None, Some(parameter), None, None, None, None, None) => {
                        Ok(RawArgument::Parameter { parameter })
                    }
                    (None, None, Some(spread), None, None, None, None) => {
                        Ok(RawArgument::Spread { spread })
                    }
                    (None, None, None, Some(json_parameter), None, None, None) => {
                        Ok(RawArgument::JsonParameter { json_parameter })
                    }
                    (None, None, None, None, Some(true), None, None) => {
                        Ok(RawArgument::JsonArguments)
                    }
                    (None, None, None, None, Some(false), None, None) => {
                        Err(serde::de::Error::custom("json_arguments must be true"))
                    }
                    (None, None, None, None, None, Some(when_present), Some(append)) => {
                        Ok(RawArgument::WhenPresent {
                            when_present,
                            append,
                        })
                    }
                    _ => Err(serde::de::Error::custom(
                        "expected exactly one literal, parameter, spread, json_parameter, json_arguments:true, or when_present with append",
                    )),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Whether this build was compiled with the named gated feature.
fn build_feature_enabled(feature: &str) -> bool {
    (feature == "remote" && cfg!(feature = "remote"))
        || (feature == "desktop" && cfg!(feature = "desktop"))
}

fn parse_manifest(raw: &str) -> Result<Manifest> {
    let raw: RawManifest = serde_json::from_str(raw)?;
    if raw.name.trim().is_empty() || raw.binary.trim().is_empty() {
        bail!("manifest name and binary must be nonempty");
    }
    if raw.binary.contains('\0') {
        bail!("manifest binary cannot contain a NUL byte");
    }
    let mut phrases = BTreeSet::new();
    for phrase in &raw.phrases {
        if phrase.trim().is_empty() || phrase == &raw.name || !phrases.insert(phrase) {
            bail!(
                "manifest {:?} has an empty or duplicate phrase {phrase:?}",
                raw.name
            );
        }
    }
    if let Some(feature) = &raw.build_feature {
        if !KNOWN_BUILD_FEATURES.contains(&feature.as_str()) {
            bail!(
                "manifest {:?} declares unknown build_feature {feature:?}; expected one of {KNOWN_BUILD_FEATURES:?}",
                raw.name
            );
        }
    }
    let parameters = raw.parameters;
    for (name, spec) in &parameters {
        if name.trim().is_empty() {
            bail!("parameter names must be nonempty");
        }
        let is_list = spec.kind == ParameterType::List;
        if is_list != spec.item_type.is_some() {
            bail!(
                "parameter {name:?}: list requires an items type, and scalar types cannot have one"
            );
        }
        if spec.item_type == Some(ParameterType::List) {
            bail!("parameter {name:?}: nested lists are not supported");
        }
        if spec
            .item_type
            .is_some_and(|item| matches!(item, ParameterType::Json | ParameterType::Record))
        {
            bail!(
                "parameter {name:?}: json and record list items require json_arguments rather than spread"
            );
        }
        if !is_list && spec.min_items.is_some() {
            bail!("parameter {name:?}: min_items only applies to lists");
        }
        let token_rules = !spec.allow_first_token.is_empty() || !spec.deny_first_token.is_empty();
        if token_rules && (!is_list || spec.item_type != Some(ParameterType::String)) {
            bail!("parameter {name:?}: first-token rules apply only to string lists");
        }
        if spec.forbid_option_items && (!is_list || spec.item_type != Some(ParameterType::String)) {
            bail!("parameter {name:?}: forbid_option_items applies only to string lists");
        }
        validate_parameter_fields(spec.kind, spec.fields.as_ref(), name)?;
    }
    if let Some(contract) = &raw.contract {
        contract.validate()?;
        if contract.backend == AppletBackend::InProcess && raw.binary != "@execute-applet" {
            bail!("in_process backend requires binary \"@execute-applet\"");
        }
        if contract.backend == AppletBackend::ProgramModule && raw.binary != "@program-module" {
            bail!("program_module backend requires binary \"@program-module\"");
        }
        if contract.backend == AppletBackend::Remote && raw.binary != "@remote-applet" {
            bail!("remote backend requires binary \"@remote-applet\"");
        }
    }
    validate_cache_dependencies(&raw.cache_dependencies, raw.cacheable)?;
    if let Some(stdio) = &raw.stdio {
        stdio.validate()?;
        if raw.builtin.is_some() {
            bail!("stdio defaults cannot be declared for a runtime builtin");
        }
        if raw.contract.as_ref().is_some_and(|contract| {
            contract.backend != AppletBackend::Command
                && (stdio
                    .stdout
                    .is_some_and(|mode| mode != StdioDisposition::Capture)
                    || stdio
                        .stderr
                        .is_some_and(|mode| mode != StdioDisposition::Capture))
        }) {
            bail!("non-Capture stdio dispositions are only supported by the command backend");
        }
    }
    let arguments = raw
        .arguments
        .into_iter()
        .map(|node| convert_argument(node, &parameters))
        .collect::<Result<Vec<_>>>()?;
    let manifest = Manifest {
        name: raw.name,
        phrases: raw.phrases,
        binary: raw.binary,
        builtin: raw.builtin,
        accepts_stdin: raw.accepts_stdin,
        cacheable: raw.cacheable,
        cache_dependencies: raw.cache_dependencies,
        uncertain_on_failure: raw.uncertain_on_failure,
        contract: raw.contract,
        build_feature: raw.build_feature,
        stdio: raw.stdio,
        origin: None,
        parameters,
        arguments,
    };
    validate_builtin(&manifest)?;
    validate_manifest_ports(&manifest)?;
    if manifest.uncertain_on_failure && (manifest.cacheable || manifest.builtin.is_some()) {
        bail!("uncertain_on_failure requires a non-cacheable external applet");
    }
    validate_argument_usage(&manifest)?;
    Ok(manifest)
}

fn validate_builtin(manifest: &Manifest) -> Result<()> {
    match manifest.builtin {
        None => Ok(()),
        Some(Builtin::Cd) => {
            if manifest.binary != "cd"
                || manifest.accepts_stdin
                || manifest.cacheable
                || manifest.parameters.len() != 1
                || manifest.parameters.get("path").is_none_or(|path| {
                    path.kind != ParameterType::String
                        || !path.required
                        || path.item_type.is_some()
                        || path.min_items.is_some()
                })
                || !matches!(
                    manifest.arguments.as_slice(),
                    [ArgumentNode::Parameter { parameter }] if parameter == "path"
                )
            {
                bail!(
                    "cd builtin must use binary cd, one required string path parameter, no stdin or caching, and map only path as its argument"
                );
            }
            Ok(())
        }
    }
}

fn validate_parameter_fields(
    kind: ParameterType,
    fields: Option<&BTreeMap<String, ParameterFieldSpec>>,
    name: &str,
) -> Result<()> {
    match (kind, fields) {
        // A declared empty record is a valid zero-field value, such as a
        // button's clicked signal. `None` still means the schema is missing.
        (ParameterType::Record, Some(fields)) => {
            for (field_name, field) in fields {
                if field_name.trim().is_empty() {
                    bail!("record parameter {name:?} has an empty field name");
                }
                if matches!(field.kind, ParameterType::List | ParameterType::Scalar) {
                    bail!(
                        "record field {field_name:?} in parameter {name:?} has unsupported type {}",
                        field.kind
                    );
                }
                validate_parameter_fields(
                    field.kind,
                    field.fields.as_ref(),
                    &format!("{name}.{field_name}"),
                )?;
            }
            Ok(())
        }
        (ParameterType::Record, None) => {
            bail!("record parameter {name:?} requires fields")
        }
        (_, Some(_)) => bail!("fields only apply to record parameter {name:?}"),
        (_, None) => Ok(()),
    }
}

fn validate_cache_dependencies(dependencies: &[String], cacheable: bool) -> Result<()> {
    const MAX_DEPENDENCIES: usize = 128;
    const MAX_PATH_BYTES: usize = 4096;
    const MAX_TOTAL_BYTES: usize = 64 * 1024;
    if dependencies.len() > MAX_DEPENDENCIES {
        bail!("cache_dependencies cannot contain more than {MAX_DEPENDENCIES} paths");
    }
    if !dependencies.is_empty() && !cacheable {
        bail!("cache_dependencies require cacheable:true");
    }
    let mut unique = BTreeSet::new();
    let mut total_bytes = 0usize;
    for dependency in dependencies {
        if dependency.is_empty() || dependency.len() > MAX_PATH_BYTES || dependency.contains('\0') {
            bail!(
                "cache dependency paths must be nonempty, NUL-free, and at most {MAX_PATH_BYTES} bytes"
            );
        }
        let path = Path::new(dependency);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir
                        | std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            bail!("cache dependency {dependency:?} must be a normalized relative path");
        }
        if !unique.insert(dependency) {
            bail!("cache_dependencies must not contain duplicate paths");
        }
        total_bytes = total_bytes
            .checked_add(dependency.len())
            .ok_or_else(|| anyhow!("cache dependency declaration size overflow"))?;
    }
    if total_bytes > MAX_TOTAL_BYTES {
        bail!("cache_dependencies exceeds {MAX_TOTAL_BYTES} total bytes");
    }
    Ok(())
}

fn validate_manifest_ports(manifest: &Manifest) -> Result<()> {
    let Some(contract) = manifest.execution_contract() else {
        return Ok(());
    };
    let Some(ports) = contract.ports.as_ref() else {
        return Ok(());
    };

    if manifest.accepts_stdin && manifest.parameters.contains_key("stdin") {
        bail!("manifest parameter \"stdin\" conflicts with the special stdin port");
    }
    let has_stdin_port = ports.inputs.contains_key("stdin") && manifest.accepts_stdin;
    if ports.inputs.contains_key("stdin") && !manifest.accepts_stdin {
        bail!("contract stdin port requires accepts_stdin:true");
    }
    let declared_parameters = ports
        .inputs
        .iter()
        .filter(|(name, _)| !(has_stdin_port && name.as_str() == "stdin"))
        .map(|(name, _)| name)
        .cloned()
        .collect::<BTreeSet<_>>();
    let manifest_parameters = manifest.parameters.keys().cloned().collect::<BTreeSet<_>>();
    if declared_parameters != manifest_parameters {
        let missing = manifest_parameters
            .difference(&declared_parameters)
            .cloned()
            .collect::<Vec<_>>();
        let extra = declared_parameters
            .difference(&manifest_parameters)
            .cloned()
            .collect::<Vec<_>>();
        bail!(
            "contract input ports must match manifest parameters exactly (missing: {missing:?}; unknown: {extra:?})"
        );
    }
    for (name, spec) in &manifest.parameters {
        let port = &ports.inputs[name];
        if port.encoding.is_some() {
            bail!("parameter input port {name:?} carries a typed value, not an encoded stream");
        }
        let expected = parameter_value_schema(spec)?;
        if !port.schema.is_assignable_to(&expected) {
            bail!("input port {name:?} schema does not match the manifest parameter schema");
        }
    }
    if let Some(stdin) = ports.inputs.get("stdin") {
        crate::contracts::validate_stdin_port(stdin)?;
    }

    let output_root = contract
        .result_envelope_schema()
        .or_else(|| contract.output.as_ref().map(output_value_schema))
        .unwrap_or_else(|| leaf_value_schema(OutputType::Bytes));
    if ports.outputs.len() > 1 && ports.outputs.values().any(|port| port.pointer.is_none()) {
        bail!("multiple output ports require an explicit JSON pointer for each output");
    }
    for (name, port) in &ports.outputs {
        if port.delivery == PortDelivery::Events {
            bail!(
                "output port {name:?} must use single or records delivery; use events namespace for events"
            );
        }
        let pointer = port.pointer.as_deref().unwrap_or("");
        if let Some(actual) = crate::contracts::schema_at_pointer(&output_root, pointer)? {
            let actual =
                if port.delivery == PortDelivery::Records && actual.kind == OutputType::List {
                    *actual
                        .items
                        .ok_or_else(|| anyhow!("list output schema is missing items"))?
                } else {
                    actual
                };
            if actual != port.schema {
                bail!("output port {name:?} schema does not match its JSON pointer target");
            }
        }
    }
    Ok(())
}

fn schema_accepts_input_kind(schema: &ValueSchema, kind: InputType) -> bool {
    match schema.kind {
        OutputType::Bytes => false,
        OutputType::Text | OutputType::Path => kind == InputType::Text,
        OutputType::Integer => kind == InputType::Integer,
        OutputType::Number => matches!(kind, InputType::Integer | InputType::Json),
        OutputType::Boolean => kind == InputType::Boolean,
        OutputType::Json => true,
        OutputType::Record => kind == InputType::Json,
        OutputType::List => {
            kind == InputType::Json
                || (kind == InputType::TextList
                    && schema.items.as_deref().is_some_and(|item| {
                        matches!(item.kind, OutputType::Text | OutputType::Path)
                    }))
        }
    }
}

fn schema_accepts_reference(schema: &ValueSchema, kind: InputType) -> bool {
    if kind == InputType::Json {
        // JSON may resolve to any structured value except raw bytes. Defer the
        // value-shape check until inputs have been resolved.
        return schema.kind != OutputType::Bytes;
    }
    schema_accepts_input_kind(schema, kind)
}

/// Validate the known shape around unresolved references. Literal siblings
/// still receive their normal schema checks, while JSON references defer the
/// concrete value check until runtime resolution.
fn validate_port_value_with_references(
    schema: &ValueSchema,
    value: &Value,
    bindings: &BTreeMap<String, InputType>,
) -> Result<()> {
    if schema.kind == OutputType::Json {
        // JSON accepts all literal values and every declared input type; the
        // unresolved references cannot violate a narrower JSON schema.
        return Ok(());
    }
    match (schema.kind, value) {
        (_, Value::Reference(reference)) => {
            let kind = bindings.get(reference).copied().unwrap_or(InputType::Text);
            if !schema_accepts_reference(schema, kind) {
                bail!(
                    "reference {reference:?} of type {kind} does not satisfy {:?} schema",
                    schema.kind
                );
            }
            Ok(())
        }
        (OutputType::Record, Value::Record(fields)) => {
            for name in &schema.required {
                if !fields.contains_key(name) {
                    bail!("record is missing required field {name:?}");
                }
            }
            for (name, value) in fields {
                let Some(field_schema) = schema.fields.get(name) else {
                    bail!("record contains undeclared field {name:?}");
                };
                validate_port_value_with_references(field_schema, value, bindings)
                    .with_context(|| format!("validating record field {name:?}"))?;
            }
            Ok(())
        }
        (OutputType::List, Value::List(items)) => {
            let item_schema = schema
                .items
                .as_deref()
                .ok_or_else(|| anyhow!("list schema is missing items"))?;
            for (index, value) in items.iter().enumerate() {
                validate_port_value_with_references(item_schema, value, bindings)
                    .with_context(|| format!("validating list item {index}"))?;
            }
            Ok(())
        }
        _ => schema.validate_value(value),
    }
}

fn contains_parameter_reference(value: &Value) -> bool {
    match value {
        Value::Reference(_) => true,
        Value::List(values) => values.iter().any(contains_parameter_reference),
        Value::Record(values) => values.values().any(contains_parameter_reference),
        Value::String(_) | Value::Integer(_) | Value::Bool(_) | Value::Null | Value::Number(_) => {
            false
        }
    }
}

fn parameter_value_schema(spec: &ParameterSpec) -> Result<ValueSchema> {
    if spec.kind == ParameterType::List {
        let item_type = spec.item_type.expect("list parameter validated");
        let item = parameter_field_value_schema(item_type, None)?;
        return Ok(ValueSchema {
            kind: OutputType::List,
            nullable: false,
            items: Some(Box::new(item)),
            fields: BTreeMap::new(),
            required: Vec::new(),
            enum_values: None,
        });
    }
    parameter_field_value_schema(spec.kind, spec.fields.as_ref())
}

#[cfg(not(target_arch = "wasm32"))]
fn parameter_input_kind(spec: &ParameterSpec) -> Result<InputType> {
    Ok(match spec.kind {
        ParameterType::String | ParameterType::Path => InputType::Text,
        ParameterType::Integer => InputType::Integer,
        ParameterType::Boolean => InputType::Boolean,
        ParameterType::Json | ParameterType::Record => InputType::Json,
        ParameterType::List
            if matches!(
                spec.item_type,
                Some(ParameterType::String | ParameterType::Path | ParameterType::Scalar)
            ) =>
        {
            InputType::TextList
        }
        ParameterType::List => {
            bail!("program modules support only text-valued list parameters")
        }
        ParameterType::Scalar => bail!("scalar is only valid as a parameter-list item type"),
    })
}

fn parameter_field_value_schema(
    kind: ParameterType,
    fields: Option<&BTreeMap<String, ParameterFieldSpec>>,
) -> Result<ValueSchema> {
    let kind = match kind {
        ParameterType::String => OutputType::Text,
        ParameterType::Path => OutputType::Path,
        ParameterType::Integer => OutputType::Integer,
        ParameterType::Boolean => OutputType::Boolean,
        ParameterType::Json => OutputType::Json,
        ParameterType::Record => OutputType::Record,
        ParameterType::Scalar => bail!("mixed scalar parameter lists have no single port schema"),
        ParameterType::List => bail!("nested parameter lists are not supported"),
    };
    let mut value_fields = BTreeMap::new();
    let mut required = Vec::new();
    if kind == OutputType::Record {
        for (name, field) in
            fields.ok_or_else(|| anyhow!("record parameter schema is missing fields"))?
        {
            value_fields.insert(
                name.clone(),
                parameter_field_value_schema(field.kind, field.fields.as_ref())?,
            );
            if field.required {
                required.push(name.clone());
            }
        }
    }
    Ok(ValueSchema {
        kind,
        nullable: false,
        items: None,
        fields: value_fields,
        required,
        enum_values: None,
    })
}

fn output_value_schema(output: &OutputSchema) -> ValueSchema {
    ValueSchema {
        kind: output.kind,
        nullable: false,
        items: output.items.clone(),
        fields: output.fields.clone(),
        required: output.required.clone(),
        enum_values: output.enum_values.clone(),
    }
}

fn leaf_value_schema(kind: OutputType) -> ValueSchema {
    ValueSchema {
        kind,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
    }
}

fn output_type_name(kind: OutputType) -> &'static str {
    match kind {
        OutputType::Bytes => "bytes",
        OutputType::Text => "text",
        OutputType::Path => "path",
        OutputType::Integer => "integer",
        OutputType::Number => "number",
        OutputType::Boolean => "boolean",
        OutputType::Json => "json",
        OutputType::Record => "record",
        OutputType::List => "list",
    }
}

fn convert_argument(
    raw: RawArgument,
    parameters: &BTreeMap<String, ParameterSpec>,
) -> Result<ArgumentNode> {
    Ok(match raw {
        RawArgument::Literal { literal } => {
            if literal.contains('\0') {
                bail!("literal argument cannot contain a NUL byte");
            }
            ArgumentNode::Literal { literal }
        }
        RawArgument::Parameter { parameter } => {
            let spec = parameters.get(&parameter).ok_or_else(|| {
                anyhow!("argument mapping references undeclared parameter {parameter:?}")
            })?;
            if spec.kind == ParameterType::List {
                bail!("list parameter {parameter:?} must use spread");
            }
            if matches!(spec.kind, ParameterType::Json | ParameterType::Record) {
                bail!(
                    "json or record parameter {parameter:?} must use json_parameter or json_arguments"
                );
            }
            ArgumentNode::Parameter { parameter }
        }
        RawArgument::Spread { spread } => {
            let spec = parameters.get(&spread).ok_or_else(|| {
                anyhow!("argument mapping references undeclared parameter {spread:?}")
            })?;
            if spec.kind != ParameterType::List {
                bail!("spread parameter {spread:?} must be a list");
            }
            ArgumentNode::Spread { spread }
        }
        RawArgument::JsonParameter { json_parameter } => {
            let spec = parameters.get(&json_parameter).ok_or_else(|| {
                anyhow!("JSON argument mapping references undeclared parameter {json_parameter:?}")
            })?;
            if !matches!(spec.kind, ParameterType::Json | ParameterType::Record) {
                bail!(
                    "json_parameter requires a json or record parameter, got {}",
                    spec.kind
                );
            }
            ArgumentNode::JsonParameter { json_parameter }
        }
        RawArgument::JsonArguments => ArgumentNode::JsonArguments,
        RawArgument::PrefixedParameter { prefix, parameter } => {
            if prefix.contains('\0') {
                bail!("prefix argument cannot contain a NUL byte");
            }
            let spec = parameters.get(&parameter).ok_or_else(|| {
                anyhow!("argument mapping references undeclared parameter {parameter:?}")
            })?;
            if !matches!(spec.kind, ParameterType::String | ParameterType::Path) {
                bail!("prefixed parameter {parameter:?} must be a string or path");
            }
            ArgumentNode::PrefixedParameter { prefix, parameter }
        }
        RawArgument::WhenPresent {
            when_present,
            append,
        } => {
            if !parameters.contains_key(&when_present) {
                bail!("conditional references undeclared parameter {when_present:?}");
            }
            ArgumentNode::WhenPresent {
                when_present,
                append: append
                    .into_iter()
                    .map(|n| convert_argument(n, parameters))
                    .collect::<Result<_>>()?,
            }
        }
    })
}

fn validate_argument_usage(manifest: &Manifest) -> Result<()> {
    fn visit<'a>(
        nodes: &'a [ArgumentNode],
        guards: &mut Vec<&'a str>,
        manifest: &Manifest,
    ) -> Result<()> {
        for node in nodes {
            match node {
                ArgumentNode::Literal { .. } | ArgumentNode::JsonArguments => {}
                ArgumentNode::Parameter { parameter }
                | ArgumentNode::PrefixedParameter { parameter, .. }
                | ArgumentNode::Spread { spread: parameter }
                | ArgumentNode::JsonParameter {
                    json_parameter: parameter,
                } => {
                    if !manifest.parameters[parameter].required
                        && !guards.contains(&parameter.as_str())
                    {
                        bail!(
                            "optional parameter {parameter:?} must be emitted within its own when_present condition"
                        );
                    }
                }
                ArgumentNode::WhenPresent {
                    when_present,
                    append,
                } => {
                    guards.push(when_present);
                    visit(append, guards, manifest)?;
                    guards.pop();
                }
            }
        }
        Ok(())
    }
    visit(&manifest.arguments, &mut Vec::new(), manifest)
}

impl std::fmt::Display for ParameterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::String => "string",
            Self::Path => "path",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::List => "list",
            Self::Json => "json",
            Self::Record => "record",
            Self::Scalar => "string, integer, or boolean",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn the_borrowed_builder_expands_a_home_path_into_owned_argv() {
        let registry = Registry::from_sources(&[serde_json::json!({
            "name": "reader.path",
            "binary": "reader",
            "accepts_stdin": false,
            "cacheable": false,
            "parameters": { "file": { "type": "path", "required": true } },
            "arguments": [{ "parameter": "file" }]
        })
        .to_string()])
        .unwrap();
        let manifest = registry.resolve("reader.path").unwrap();
        let supplied = BTreeMap::from([("file".to_owned(), Value::String("~/a.txt".to_owned()))]);
        let borrowed = manifest.build_arguments_borrowed(&supplied).unwrap();
        let expected = crate::home::expand_home_path("~/a.txt")
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();
        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0], expected.as_str());
        assert!(
            matches!(borrowed[0], Cow::Owned(_)),
            "an expanded path is owned"
        );
    }

    fn write_manifest(dir: &Path, name: &str, json: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn catalog_resolves_canonical_names_and_phrases() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "one.json",
            r#"{"name":"say","phrases":["say text"],"binary":"printf","arguments":[]}"#,
        );
        let registry = Registry::load(dir.path()).unwrap();
        assert_eq!(registry.resolve("say").unwrap().binary, "printf");
        assert_eq!(registry.resolve("say text").unwrap().name, "say");
        assert_eq!(registry.manifests().unwrap().len(), 1);
        assert!(registry.resolve("missing").is_err());
    }

    /// Applets that need a gated build feature whose backend alone does not say so.
    /// Add an entry here when a gated applet is added; the remote backend is covered
    /// without a list entry.
    const GATED_APPLETS: &[(&str, &str)] = &[("desktop.quickshell", "desktop")];

    /// Every manifest that needs a gated feature must declare it as `build_feature`, so
    /// a new remote or renderer applet cannot reach a lean build without the marker.
    /// Reads the shipped JSON directly, so the test does not depend on the registry.
    #[test]
    fn every_gated_manifest_declares_its_build_feature() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let mut checked = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            let name = value["name"].as_str().unwrap_or_default().to_owned();
            let backend = value["contract"]["backend"].as_str().unwrap_or("command");
            let needed = if backend == "remote" {
                Some("remote")
            } else {
                GATED_APPLETS
                    .iter()
                    .find(|(gated, _)| *gated == name)
                    .map(|(_, feature)| *feature)
            };
            let Some(needed) = needed else {
                continue;
            };
            checked += 1;
            assert_eq!(
                value["build_feature"].as_str(),
                Some(needed),
                "{} needs the `{needed}` build feature and must declare \"build_feature\": \"{needed}\"",
                path.display()
            );
        }
        assert!(
            checked >= 3,
            "expected the remote and renderer applets to be checked, saw {checked}"
        );
    }

    /// Gated applets are available exactly when this build compiled their feature, and
    /// unavailable applets carry the full-build refusal.
    #[test]
    fn gated_applets_report_availability_for_this_build() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let registry = Registry::load_default(&dir, ManifestCheck::Lazy).unwrap();
        let quickshell = registry.resolve("desktop.quickshell").unwrap();
        assert_eq!(quickshell.build_available(), cfg!(feature = "desktop"));
        let remote = registry.resolve("desktop.clock_remote").unwrap();
        assert_eq!(remote.build_available(), cfg!(feature = "remote"));
        assert_eq!(
            remote.build_unavailable_message().is_some(),
            !cfg!(feature = "remote")
        );
        assert!(registry.resolve("printf").unwrap().build_available());
    }

    #[test]
    fn bundled_registry_matches_a_full_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let full = Registry::load(&dir).unwrap();
        let bundled = Registry::load_default(&dir, ManifestCheck::Lazy).unwrap();
        assert!(
            matches!(*bundled.data.manifests, Manifests::Bundled { .. }),
            "the shipped directory should be served by the bundled index"
        );
        let full_list = full.manifests().unwrap();
        let bundled_list = bundled.manifests().unwrap();
        assert_eq!(full_list.len(), bundled_list.len());
        for (left, right) in full_list.iter().zip(&bundled_list) {
            assert_eq!(
                serde_json::to_value(left).unwrap(),
                serde_json::to_value(right).unwrap()
            );
            assert_eq!(left.origin, right.origin);
        }
        assert_eq!(full.applet_names(), bundled.applet_names());
        for key in full.data.names.keys() {
            assert_eq!(
                full.resolve(key).unwrap().name,
                bundled.resolve(key).unwrap().name,
                "{key}"
            );
        }
    }

    fn copy_shipped_manifests(dir: &Path) {
        for entry in fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap()
        {
            let entry = entry.unwrap();
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
            }
        }
    }

    #[test]
    fn a_full_check_of_a_changed_default_directory_takes_the_full_load_and_its_errors() {
        let dir = tempdir().unwrap();
        copy_shipped_manifests(dir.path());
        fs::write(dir.path().join("awk.json"), "{").unwrap();
        let full_error = format!("{:#}", Registry::load(dir.path()).err().unwrap());
        let check_error = format!(
            "{:#}",
            Registry::load_default(dir.path(), ManifestCheck::Full)
                .err()
                .unwrap()
        );
        assert_eq!(check_error, full_error);
    }

    #[test]
    fn a_lazily_checked_changed_manifest_fails_closed_on_first_use() {
        let dir = tempdir().unwrap();
        copy_shipped_manifests(dir.path());
        let path = dir.path().join("awk.json");
        let mut changed = fs::read_to_string(&path).unwrap();
        changed.push(' ');
        fs::write(&path, changed).unwrap();

        let registry = Registry::load_default(dir.path(), ManifestCheck::Lazy).unwrap();
        assert!(matches!(
            *registry.data.manifests,
            Manifests::Bundled { .. }
        ));
        // Unchanged manifests still resolve from the bundled index.
        assert_eq!(registry.resolve("cat").unwrap().name, "cat");
        let error = format!("{:#}", registry.resolve("awk").err().unwrap());
        assert!(error.contains("differs from the bundled copy"), "{error}");
        // The registry never switches to disk content, so enumeration fails closed too.
        assert!(registry.manifests().is_err());

        // A full check of the same directory loads the edited file instead.
        let full = Registry::load_default(dir.path(), ManifestCheck::Full).unwrap();
        assert!(matches!(*full.data.manifests, Manifests::Parsed(_)));
        assert_eq!(full.resolve("awk").unwrap().name, "awk");
    }

    #[test]
    fn rejects_unknown_fields_bad_mappings_and_duplicate_aliases() {
        let dir = tempdir().unwrap();
        let values_dir = tempdir().unwrap();
        let path = write_manifest(
            values_dir.path(),
            "bad.json",
            r#"{"name":"x","binary":"x","surprise":true,"arguments":[]}"#,
        );
        assert!(Registry::load(&path).is_err());

        let path = write_manifest(
            dir.path(),
            "bad.json",
            r#"{"name":"x","binary":"x","parameters":{"p":{"type":"string"}},"arguments":[{"parameter":"nope"}]}"#,
        );
        assert!(Registry::load(&path).is_err());

        write_manifest(
            dir.path(),
            "one.json",
            r#"{"name":"x","phrases":["same"],"binary":"x","arguments":[]}"#,
        );
        write_manifest(
            dir.path(),
            "two.json",
            r#"{"name":"y","phrases":["same"],"binary":"y","arguments":[]}"#,
        );
        assert!(Registry::load(dir.path()).is_err());
    }

    #[test]
    fn validates_references_then_builds_conditional_spread_argv_without_shell_expansion() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "commit.json",
            r#"{
          "name":"commit","binary":"git",
          "parameters":{"message":{"type":"string","required":true},"files":{"type":"list","items":"string","min_items":1}},
          "arguments":[{"literal":"commit"},{"literal":"-m"},{"parameter":"message"},{"when_present":"files","append":[{"literal":"--only"},{"literal":"--"},{"spread":"files"}]}]
        }"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("commit")
            .unwrap()
            .clone();
        let invocation = Invocation {
            operation: "commit".into(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::from([
                ("message".into(), Value::Reference("message".into())),
                (
                    "files".into(),
                    Value::List(vec![Value::Reference("files".into())]),
                ),
            ]),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        };
        assert!(manifest.validate(&invocation).is_ok());
        let values = BTreeMap::from([
            ("message".into(), Value::String("hello; echo unsafe".into())),
            (
                "files".into(),
                Value::List(vec![
                    Value::String("file with spaces.txt".into()),
                    Value::String("--flag".into()),
                ]),
            ),
        ]);
        assert_eq!(
            manifest.build_arguments(&values).unwrap(),
            [
                "commit",
                "-m",
                "hello; echo unsafe",
                "--only",
                "--",
                "file with spaces.txt",
                "--flag"
            ]
        );
        assert!(manifest
            .build_arguments(&BTreeMap::from([
                ("message".into(), Value::String("ok".into())),
                ("files".into(), Value::List(vec![]))
            ]))
            .is_err());
        assert!(manifest
            .build_arguments(&BTreeMap::from([("message".into(), Value::Integer(7))]))
            .is_err());
    }

    #[test]
    fn catches_missing_and_mistyped_invocation_parameters() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "say.json",
            r#"{"name":"say","binary":"printf","parameters":{"text":{"type":"string","required":true}},"arguments":[{"parameter":"text"}]}"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("say")
            .unwrap()
            .clone();
        let missing = Invocation {
            operation: "say".into(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::new(),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        };
        assert!(manifest.validate(&missing).is_err());
        let wrong = Invocation {
            operation: "say".into(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::from([("text".into(), Value::Integer(1))]),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        };
        assert!(manifest.validate(&wrong).is_err());
    }

    #[test]
    fn list_item_references_are_deferred_until_values_are_resolved() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "files.json",
            r#"{"name":"files","binary":"cat","parameters":{"paths":{"type":"list","items":"string","required":true}},"arguments":[{"spread":"paths"}]}"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("files")
            .unwrap()
            .clone();
        let invocation = Invocation {
            operation: "files".into(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::from([(
                "paths".into(),
                Value::List(vec![Value::Reference("path".into())]),
            )]),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        };
        assert!(manifest.validate(&invocation).is_ok());
        assert!(manifest
            .build_arguments(&BTreeMap::from([(
                "paths".into(),
                Value::List(vec![Value::Reference("path".into())]),
            )]))
            .is_err());
        assert!(manifest
            .build_arguments(&BTreeMap::from([(
                "paths".into(),
                Value::List(vec![Value::String("valid.txt".into())]),
            )]))
            .is_ok());
    }

    #[test]
    fn references_only_defer_for_string_destinations() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "typed.json",
            r#"{"name":"typed","binary":"tool","parameters":{"text":{"type":"string"},"number":{"type":"integer"},"flag":{"type":"boolean"},"strings":{"type":"list","items":"string"},"numbers":{"type":"list","items":"integer"}},"arguments":[]}"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("typed")
            .unwrap()
            .clone();
        let invocation = |name: &str, value: Value| Invocation {
            operation: "typed".into(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::from([(name.into(), value)]),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        };

        assert!(manifest
            .validate(&invocation("text", Value::Reference("result".into())))
            .is_ok());
        assert!(manifest
            .validate(&invocation(
                "strings",
                Value::List(vec![Value::Reference("result".into())]),
            ))
            .is_ok());
        for (name, value) in [
            ("number", Value::Reference("result".into())),
            ("flag", Value::Reference("result".into())),
            ("strings", Value::Reference("result".into())),
            (
                "numbers",
                Value::List(vec![Value::Reference("result".into())]),
            ),
        ] {
            assert!(manifest.validate(&invocation(name, value)).is_err());
        }
        assert!(manifest
            .build_arguments(&BTreeMap::from([(
                "text".into(),
                Value::Reference("result".into()),
            )]))
            .is_err());
    }

    #[test]
    fn optional_parameter_must_be_guarded_by_its_own_condition() {
        let dir = tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            "bad-guard.json",
            r#"{"name":"bad","binary":"tool","parameters":{"first":{"type":"string"},"second":{"type":"string"}},"arguments":[{"when_present":"second","append":[{"parameter":"first"}]}]}"#,
        );
        assert!(Registry::load(&path).is_err());
    }

    #[test]
    fn unix_cli_manifests_build_plain_argv_and_resolve_tail_alias() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let registry = Registry::load(&manifest_dir).unwrap();

        assert_eq!(registry.resolve("tail").unwrap().name, "tail");
        assert_eq!(registry.resolve("follow file").unwrap().name, "tail.follow");
        assert_eq!(
            registry
                .resolve("tail")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "path".into(),
                    Value::String("file".into()),
                )]))
                .unwrap(),
            ["--", "file"]
        );
        assert_eq!(
            registry
                .resolve("tail")
                .unwrap()
                .build_arguments(&BTreeMap::from([
                    (
                        "flags".into(),
                        Value::List(vec![
                            Value::String("-n".into()),
                            Value::String("0".into()),
                            Value::String("-F".into()),
                        ]),
                    ),
                    ("path".into(), Value::String("file".into())),
                ]))
                .unwrap(),
            ["-n", "0", "-F", "--", "file"]
        );
        assert_eq!(
            registry
                .resolve("tail.follow")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "path".into(),
                    Value::String("file".into()),
                )]))
                .unwrap(),
            ["-n", "0", "-F", "--", "file"]
        );
        assert_eq!(registry.resolve("cd").unwrap().builtin, Some(Builtin::Cd));
        assert_eq!(
            registry
                .resolve("cd")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "path".into(),
                    Value::String("folder with spaces".into()),
                )]))
                .unwrap(),
            ["folder with spaces"]
        );
        assert!(registry
            .resolve("pwd")
            .unwrap()
            .build_arguments(&BTreeMap::new())
            .unwrap()
            .is_empty());
        assert_eq!(
            registry
                .resolve("ls")
                .unwrap()
                .build_arguments(&BTreeMap::from([
                    (
                        "flags".into(),
                        Value::List(vec![Value::String("-a".into())]),
                    ),
                    ("path".into(), Value::String("-leading-name".into()),),
                ]))
                .unwrap(),
            ["-a", "--", "-leading-name"]
        );
        assert_eq!(
            registry
                .resolve("wc")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "flags".into(),
                    Value::List(vec![Value::String("-l".into())]),
                )]))
                .unwrap(),
            ["-l"]
        );
        assert!(!registry.resolve("wc").unwrap().cacheable);
        assert_eq!(
            registry
                .resolve("tr")
                .unwrap()
                .build_arguments(&BTreeMap::from([
                    ("from".into(), Value::String("[:lower:]".into())),
                    ("to".into(), Value::String("[:upper:]".into())),
                ]))
                .unwrap(),
            ["[:lower:]", "[:upper:]"]
        );
    }

    #[test]
    fn cd_builtin_rejects_non_path_argument_mappings() {
        let dir = tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            "bad-cd.json",
            r#"{"name":"cd","binary":"cd","builtin":"cd","parameters":{"path":{"type":"string","required":true}},"arguments":[{"literal":"ignored"}]}"#,
        );
        assert!(Registry::load(&path).is_err());
    }

    #[test]
    fn cat_path_is_optional_and_is_passed_as_a_positional_argument() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let registry = Registry::load(&manifest_dir).unwrap();
        let cat = registry.resolve("cat").unwrap();

        assert!(cat.accepts_stdin);
        assert!(!cat.cacheable);
        assert_eq!(
            cat.build_arguments(&BTreeMap::from([(
                "path".into(),
                Value::String("-input.txt".into()),
            )]))
            .unwrap(),
            ["--", "-input.txt"]
        );
        assert_eq!(cat.build_arguments(&BTreeMap::new()).unwrap(), ["--"]);
        assert_eq!(
            cat.build_arguments(&BTreeMap::from([
                ("path".into(), Value::String("first.txt".into())),
                (
                    "files".into(),
                    Value::List(vec![
                        Value::String("-second.txt".into()),
                        Value::String("third.txt".into())
                    ])
                ),
            ]))
            .unwrap(),
            ["--", "first.txt", "-second.txt", "third.txt"]
        );
    }

    #[test]
    fn rejects_nul_in_binary_literals_and_string_values() {
        let dir = tempdir().unwrap();
        for (filename, manifest) in [
            (
                "nul-binary.json",
                r#"{"name":"bad","binary":"tool\u0000bad","arguments":[]}"#,
            ),
            (
                "nul-argument.json",
                r#"{"name":"bad","binary":"tool","arguments":[{"literal":"bad\u0000arg"}]}"#,
            ),
        ] {
            let path = write_manifest(dir.path(), filename, manifest);
            assert!(Registry::load(&path).is_err(), "accepted {filename}");
        }

        let path = write_manifest(
            dir.path(),
            "strings.json",
            r#"{"name":"strings","binary":"tool","parameters":{"text":{"type":"string"},"items":{"type":"list","items":"string"}},"arguments":[]}"#,
        );
        let manifest = Registry::load(&path)
            .unwrap()
            .resolve("strings")
            .unwrap()
            .clone();
        for value in [
            Value::String("bad\0text".into()),
            Value::List(vec![Value::String("bad\0item".into())]),
        ] {
            let parameter = if matches!(value, Value::List(_)) {
                "items"
            } else {
                "text"
            };
            let invocation = Invocation {
                operation: "strings".into(),
                direct: false,
                discard_stderr: false,
                parameters: BTreeMap::from([(parameter.into(), value.clone())]),
                binding: None,
                cache: false,
                status_binding: None,
                result_binding: None,
                retry: None,
            };
            assert!(manifest.validate(&invocation).is_err());
            assert!(manifest
                .build_arguments(&BTreeMap::from([(parameter.into(), value)]))
                .is_err());
        }
    }
    #[test]
    fn opt_manifest_borrowed_argv_matches_owned_for_all_scalar_and_spread_values() {
        let dir = tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            "borrowed.json",
            r#"{"name":"check","binary":"tool","parameters":{"text":{"type":"string","required":true},"number":{"type":"integer","required":true},"flag":{"type":"boolean","required":true},"items":{"type":"list","items":"string","required":true},"extra":{"type":"list","items":"string"}},"arguments":[{"literal":"π"},{"parameter":"text"},{"parameter":"number"},{"parameter":"flag"},{"spread":"items"},{"when_present":"extra","append":[{"literal":"extra"},{"spread":"extra"}]}]}"#,
        );
        let registry = Registry::load(&path).unwrap();
        let manifest = registry.resolve("check").unwrap();
        let values = BTreeMap::from([
            ("text".into(), Value::String(String::new())),
            ("number".into(), Value::Integer(-7)),
            ("flag".into(), Value::Bool(false)),
            (
                "items".into(),
                Value::List(vec![
                    Value::String(String::new()),
                    Value::String("雪".into()),
                ]),
            ),
            (
                "extra".into(),
                Value::List(vec![Value::String("🌱".into())]),
            ),
        ]);
        let expected = ["π", "", "-7", "false", "", "雪", "extra", "🌱"];
        assert_eq!(
            manifest.build_arguments(&values).unwrap(),
            expected.map(str::to_owned)
        );
        let borrowed = manifest.build_arguments_borrowed(&values).unwrap();
        let rendered: Vec<&str> = borrowed.iter().map(|argument| argument.as_ref()).collect();
        assert_eq!(rendered, expected);
        assert!(matches!(
            &borrowed[0],
            Cow::Borrowed(value) if *value == "π"
        ));
        assert!(matches!(
            &borrowed[2],
            Cow::Owned(value) if value.as_str() == "-7"
        ));

        let mut invalid = values.clone();
        invalid.insert("text".into(), Value::String("bad\0text".into()));
        assert_eq!(
            manifest.build_arguments(&invalid).unwrap_err().to_string(),
            manifest
                .build_arguments_borrowed(&invalid)
                .unwrap_err()
                .to_string()
        );
    }
}

#[cfg(test)]
mod argument_decoder_compat {
    use super::*;

    // Retain the exact former decoder as an independent accept/reject and
    // variant-selection oracle for legacy nodes. The former deny_unknown_fields
    // setting also rejects sequence-shaped nodes; do not broaden legacy syntax.
    #[derive(Deserialize)]
    #[serde(untagged, deny_unknown_fields)]
    enum LegacyArgument {
        Literal {
            literal: String,
        },
        Parameter {
            parameter: String,
        },
        Spread {
            spread: String,
        },
        WhenPresent {
            when_present: String,
            append: Vec<LegacyArgument>,
        },
    }
    impl From<LegacyArgument> for RawArgument {
        fn from(value: LegacyArgument) -> Self {
            match value {
                LegacyArgument::Literal { literal } => Self::Literal { literal },
                LegacyArgument::Parameter { parameter } => Self::Parameter { parameter },
                LegacyArgument::Spread { spread } => Self::Spread { spread },
                LegacyArgument::WhenPresent {
                    when_present,
                    append,
                } => Self::WhenPresent {
                    when_present,
                    append: append.into_iter().map(Into::into).collect(),
                },
            }
        }
    }
    fn compare(source: &str) {
        let legacy = serde_json::from_str::<LegacyArgument>(source).map(RawArgument::from);
        let current = serde_json::from_str::<RawArgument>(source);
        assert_eq!(
            current.is_ok(),
            legacy.is_ok(),
            "acceptance differs: {source}"
        );
        if let (Ok(current), Ok(legacy)) = (current, legacy) {
            assert_eq!(
                format!("{current:?}"),
                format!("{legacy:?}"),
                "mapping differs: {source}"
            );
        }
    }
    #[test]
    fn argument_decoder_matches_legacy_objects_duplicates_sequences_and_nesting() {
        let keys = [
            "literal",
            "parameter",
            "spread",
            "when_present",
            "append",
            "extra",
        ];
        let values = [
            r#""p""#,
            r#""""#,
            r#""snow 雪\\end""#,
            "null",
            "true",
            "7",
            "1.5",
            "[]",
            r#"[{"literal":"x"}]"#,
            "{}",
        ];
        for key in keys {
            for value in values {
                compare(&format!("{{\"{key}\":{value}}}"));
                for other in keys {
                    for next in values {
                        // Repeating the same key is intentional; do not build
                        // through serde_json::Value, which would erase it.
                        compare(&format!("{{\"{key}\":{value},\"{other}\":{next}}}"));
                    }
                }
            }
        }
        for first in values {
            compare(&format!("[{first}]"));
            for second in values {
                compare(&format!("[{first},{second}]"));
                compare(&format!("[{first},{second},null]"));
            }
        }
        for source in [
            "null",
            "true",
            "7",
            r#""p""#,
            "[]",
            "{}",
            r#"{"literal":"x","parameter":"p","append":[]}"#,
            r#"{"literal":"\u0000"}"#,
            r#"{"lit\u0065ral":"escaped key"}"#,
            r#"{"literal":"x","literal":"x"}"#,
            r#"{"when_present":"p","append":[],"append":[]}"#,
        ] {
            compare(source);
        }
        let mut object = r#"{"literal":"end"}"#.to_owned();
        let mut sequence = r#"["end"]"#.to_owned();
        for _ in 0..70 {
            compare(&object);
            compare(&sequence);
            object = format!("{{\"when_present\":\"p\",\"append\":[{object}]}}");
            sequence = format!("[\"p\",[{sequence}]]");
        }
    }
    #[test]
    fn argument_decoder_matches_all_shipped_catalog_nodes() {
        fn visit(value: &serde_json::Value, extensions: &mut usize) {
            if value.get("json_arguments").is_some() {
                let source = value.to_string();
                assert_eq!(
                    value["json_arguments"], true,
                    "invalid extension node: {source}"
                );
                assert!(
                    serde_json::from_str::<LegacyArgument>(&source).is_err(),
                    "new json_arguments node unexpectedly belongs to the legacy grammar: {source}"
                );
                assert!(matches!(
                    serde_json::from_str::<RawArgument>(&source),
                    Ok(RawArgument::JsonArguments)
                ));
                *extensions += 1;
            } else if value.get("json_parameter").is_some() {
                // Extension nodes are outside the legacy grammar, so the legacy
                // oracle cannot compare them. Check the current decoder directly.
                let source = value.to_string();
                assert!(
                    value["json_parameter"].is_string(),
                    "invalid json_parameter node: {source}"
                );
                assert!(
                    serde_json::from_str::<LegacyArgument>(&source).is_err(),
                    "new json_parameter node unexpectedly belongs to the legacy grammar: {source}"
                );
                assert!(matches!(
                    serde_json::from_str::<RawArgument>(&source),
                    Ok(RawArgument::JsonParameter { .. })
                ));
                *extensions += 1;
            } else if value.get("prefix").is_some() {
                // `{"prefix": ..., "parameter": ...}` is also an extension node.
                let source = value.to_string();
                assert!(
                    value["prefix"].is_string() && value["parameter"].is_string(),
                    "invalid prefix node: {source}"
                );
                assert!(
                    serde_json::from_str::<LegacyArgument>(&source).is_err(),
                    "new prefix node unexpectedly belongs to the legacy grammar: {source}"
                );
                assert!(matches!(
                    serde_json::from_str::<RawArgument>(&source),
                    Ok(RawArgument::PrefixedParameter { .. })
                ));
                *extensions += 1;
            } else if !contains_extension(value) {
                compare(&value.to_string());
            }
            if let Some(children) = value.get("append").and_then(|v| v.as_array()) {
                for child in children {
                    visit(child, extensions);
                }
            }
        }
        // A `when_present` wrapper around an extension node is checked through
        // its children, because the legacy oracle rejects the whole wrapper.
        fn contains_extension(value: &serde_json::Value) -> bool {
            value.get("json_arguments").is_some()
                || value.get("json_parameter").is_some()
                || value.get("prefix").is_some()
                || value
                    .get("append")
                    .and_then(|v| v.as_array())
                    .is_some_and(|children| children.iter().any(contains_extension))
        }
        let mut extensions = 0;
        for entry in
            std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap()
        {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let source = std::fs::read_to_string(path).unwrap();
                let manifest: serde_json::Value = serde_json::from_str(&source).unwrap();
                for node in manifest["arguments"].as_array().unwrap() {
                    visit(node, &mut extensions);
                }
            }
        }
        assert!(
            extensions > 0,
            "catalog should exercise the new argument node"
        );
    }
}

#[cfg(test)]
mod first_token_rule_tests {
    use super::*;

    fn deny_exec() -> ParameterSpec {
        ParameterSpec {
            kind: ParameterType::List,
            item_type: Some(ParameterType::String),
            required: true,
            min_items: None,
            allow_first_token: Vec::new(),
            deny_first_token: vec!["exec".into(), "execr".into()],
            forbid_option_items: false,
            fields: None,
        }
    }

    fn option_spec() -> ParameterSpec {
        let mut spec = deny_exec();
        spec.deny_first_token.clear();
        spec.forbid_option_items = true;
        spec
    }

    #[test]
    fn option_rule_applies_to_every_item_and_allows_plain_numbers() {
        let spec = option_spec();
        let items = |list: &[&str]| -> Vec<Value> {
            list.iter()
                .map(|item| Value::String((*item).into()))
                .collect()
        };
        assert!(check_option_items("a", &spec, &items(&["workspace", "2"]), false).is_ok());
        assert!(
            check_option_items("a", &spec, &items(&["resizeactive", "-10", "0"]), false).is_ok()
        );
        assert!(check_option_items(
            "a",
            &spec,
            &items(&["resizeactive", "-10%", "10.5%"]),
            false
        )
        .is_ok());
        assert!(
            check_option_items("a", &spec, &items(&["workspace", "--batch", "x"]), false).is_err()
        );
        assert!(check_option_items("a", &spec, &items(&["workspace", "-j"]), false).is_err());
        assert!(check_option_items("a", &spec, &items(&["workspace", "1;2"]), false).is_err());
        assert!(check_option_items("a", &spec, &items(&["workspace", "a\nb"]), false).is_err());
        assert!(check_option_items("a", &spec, &items(&["workspace", "a\0b"]), false).is_err());
        assert!(check_option_items("a", &spec, &items(&["-"]), false).is_err());
        assert!(check_option_items("a", &spec, &items(&["-10x"]), false).is_err());
        let reference = [Value::Reference("word".into())];
        assert!(check_option_items("a", &spec, &reference, true).is_ok());
        assert!(check_option_items("a", &spec, &reference, false).is_err());
    }

    #[test]
    fn option_rule_is_off_unless_declared() {
        let spec = deny_exec();
        assert!(check_option_items("a", &spec, &[Value::String("--batch".into())], false).is_ok());
    }

    fn text(value: &str) -> [Value; 1] {
        [Value::String(value.into())]
    }

    #[test]
    fn deny_rule_compares_the_whole_first_item() {
        let spec = deny_exec();
        assert!(check_first_token("arguments", &spec, &[Value::Integer(1)]).is_err());
        assert!(check_first_token("arguments", &spec, &[Value::Reference("word".into())]).is_err());
        assert!(check_first_token("arguments", &spec, &text("EXEC")).is_err());
        assert!(check_first_token("arguments", &spec, &text("execr")).is_err());
        assert!(check_first_token("arguments", &spec, &text("-j")).is_err());
        assert!(check_first_token("arguments", &spec, &text("workspace")).is_ok());
        // Whole-item comparison: a sub-word match is not a refusal, and the
        // deny list is never bypassed by reading only part of the item.
        assert!(check_first_token("arguments", &spec, &text("executive")).is_ok());
        assert!(check_first_token("arguments", &spec, &[]).is_ok());
    }

    #[test]
    fn rules_refuse_blank_and_option_shaped_first_items() {
        let spec = deny_exec();
        for item in ["", "   ", "\t", "-j", " -j", "--exec"] {
            assert!(
                check_first_token("arguments", &spec, &text(item)).is_err(),
                "{item:?}"
            );
        }
    }

    #[test]
    fn allow_list_requires_a_first_item_and_matches_the_whole_item() {
        let mut spec = deny_exec();
        spec.deny_first_token.clear();
        spec.allow_first_token = vec!["workspace".into(), "exec".into()];
        assert!(check_first_token("arguments", &spec, &[]).is_err());
        assert!(check_first_token("arguments", &spec, &text("WORKSPACE")).is_ok());
        assert!(check_first_token("arguments", &spec, &text("workspace 2")).is_err());
        assert!(check_first_token("arguments", &spec, &text("exec,")).is_err());
        assert!(check_first_token("arguments", &spec, &text("unknown")).is_err());
        assert!(check_first_token("arguments", &spec, &text(" exec")).is_err());
    }
}
