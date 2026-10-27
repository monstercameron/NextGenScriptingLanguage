//! The bundled manifest set, indexed at build time.
//!
//! `src/bundled_manifests/index.rs` is generated from the manifest directory and holds
//! static metadata and lookup keys. Each body is a separate DEFLATE stream in
//! `bodies.bin`; resolving one manifest inflates only that manifest, then the typed
//! parse runs the same code as a file load.
//!
//! A registry uses the index only when the directory it was asked for holds exactly
//! the indexed file names as plain files; any other directory takes the full load.
//! Each file is compared by size and SHA-256 when its manifest is first used. A
//! mismatch fails closed consistently for checks, applet inspection, and runs.
//!
//! `generate` rebuilds all artifacts from a manifest directory after a full load has
//! validated every file. A test fails when the checked-in artifacts no longer match
//! the manifests; `EXECUTE_UPDATE_BUNDLED_INDEX=1 cargo test` rewrites them.

use crate::manifest::Registry;
use anyhow::{anyhow, bail, Context, Result};
use sha2::Digest;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::OnceLock;

const FORMAT_LINE: &str = "execute.bundled-manifests\t2";
#[cfg(test)]
const INDEX_TSV: &str = include_str!("bundled_manifests/index.tsv");
const BODIES: &[u8] = include_bytes!("bundled_manifests/bodies.bin");
include!("bundled_manifests/index.rs");
/// Separates phrases inside one index field. Unit separator: never in a manifest name.
const PHRASE_SEPARATOR: char = '\u{1f}';

/// One bundled manifest as the index describes it. `name` and `phrases` are the
/// resolution keys, so an applet resolves without inflating its body.
pub struct BundledEntry {
    file: &'static str,
    bytes: u64,
    sha256: &'static str,
    name: &'static str,
    phrases: &'static [&'static str],
    offset: usize,
    length: usize,
    body: OnceLock<std::result::Result<String, String>>,
}

impl BundledEntry {
    pub fn file(&self) -> &'static str {
        self.file
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn sha256(&self) -> &'static str {
        self.sha256
    }

    pub fn phrases(&self) -> &[&'static str] {
        self.phrases
    }
}

/// The immutable bundled index. Every registry built from it shares this one value.
/// `bodies` holds every manifest's exact source, concatenated and inflated once.
pub struct BundledIndex {
    entries: &'static [BundledEntry],
    names: &'static [(&'static str, u16)],
}

impl BundledIndex {
    pub fn entries(&self) -> &[BundledEntry] {
        self.entries
    }

    /// Canonical names and phrases mapped to entry positions.
    pub fn lookup(&self, name: &str) -> Option<usize> {
        self.names
            .binary_search_by_key(&name, |(key, _)| *key)
            .ok()
            .map(|position| usize::from(self.names[position].1))
    }

    pub fn names(&self) -> &[(&'static str, u16)] {
        self.names
    }

    /// The exact source text of one bundled manifest.
    pub fn body(&self, index: usize) -> Result<&str> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| anyhow!("bundled manifest {index} is out of range"))?;
        let end = entry
            .offset
            .checked_add(entry.length)
            .ok_or_else(|| anyhow!("bundled manifest {} has an invalid range", entry.file))?;
        let source = entry
            .body
            .get_or_init(|| -> std::result::Result<String, String> {
                let mut bodies = Vec::new();
                let compressed = BODIES.get(entry.offset..end).ok_or_else(|| {
                    format!("bundled manifest {} is outside the bodies", entry.file)
                })?;
                flate2::read::DeflateDecoder::new(compressed)
                    .read_to_end(&mut bodies)
                    .context("inflating bundled manifest bodies")
                    .map_err(|error| format!("{error:#}"))?;
                String::from_utf8(bodies)
                    .context("bundled manifest bodies are not UTF-8")
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|message: &String| anyhow!("{message}"))?
            .as_str();
        Ok(source)
    }
}

static INDEX: BundledIndex = BundledIndex {
    entries: &ENTRIES,
    names: &NAMES,
};

/// The embedded index and bodies, read once for the process.
pub fn index() -> Result<&'static BundledIndex> {
    Ok(&INDEX)
}

/// Whether `root` holds exactly the indexed files by name and plain-file type.
/// File sizes and hashes are checked when an indexed manifest is resolved. An
/// unreadable directory, a subdirectory or symlink among the JSON files, or any
/// name difference is a mismatch, and the caller falls back to the full load.
pub fn directory_matches(root: &Path, index: &BundledIndex) -> io::Result<bool> {
    directory_matches_with(root, index, &mut FsManifestDirectory)
}

trait ManifestDirectoryProbe {
    type Entry;

    fn entries(&mut self, root: &Path) -> io::Result<Vec<Self::Entry>>;
    fn name(&self, entry: &Self::Entry) -> std::ffi::OsString;
    fn is_file(&mut self, entry: &Self::Entry) -> io::Result<bool>;
    #[cfg(test)]
    fn metadata_len(&mut self, entry: &Self::Entry) -> io::Result<u64>;
}

struct FsManifestDirectory;

impl ManifestDirectoryProbe for FsManifestDirectory {
    type Entry = fs::DirEntry;

    fn entries(&mut self, root: &Path) -> io::Result<Vec<Self::Entry>> {
        fs::read_dir(root)?.collect()
    }

    fn name(&self, entry: &Self::Entry) -> std::ffi::OsString {
        entry.file_name()
    }

    fn is_file(&mut self, entry: &Self::Entry) -> io::Result<bool> {
        entry.file_type().map(|kind| kind.is_file())
    }

    #[cfg(test)]
    fn metadata_len(&mut self, entry: &Self::Entry) -> io::Result<u64> {
        entry.metadata().map(|metadata| metadata.len())
    }
}

fn directory_matches_with<P: ManifestDirectoryProbe>(
    root: &Path,
    index: &BundledIndex,
    probe: &mut P,
) -> io::Result<bool> {
    let mut found = Vec::new();
    for entry in probe.entries(root)? {
        let name = probe.name(&entry);
        if Path::new(&name)
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        if !probe.is_file(&entry)? {
            return Ok(false);
        }
        let Ok(name) = name.into_string() else {
            return Ok(false);
        };
        found.push(name);
    }
    found.sort();
    Ok(found.len() == index.entries.len()
        && found
            .iter()
            .zip(index.entries.iter())
            .all(|(name, entry)| name == entry.file))
}

/// Artifacts generated from one manifest directory.
pub struct Generated {
    pub index: String,
    pub index_rust: String,
    pub bodies: Vec<u8>,
}

/// Validate every manifest in `dir` with a full load, then describe each as an index
/// entry. Files are taken in name order, which the full load also uses.
pub fn generate(dir: &Path) -> Result<Generated> {
    Registry::load(dir).with_context(|| format!("validating manifests in {}", dir.display()))?;
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            bail!(
                "{} is not a plain file, so it cannot be indexed",
                path.display()
            );
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("manifest name {} is not UTF-8", path.display()))?;
        files.push((name, path));
    }
    files.sort();
    let mut index = format!("{FORMAT_LINE}\n");
    let mut body_data = Vec::new();
    let mut entries = Vec::new();
    let mut names = Vec::new();
    for (file, path) in &files {
        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("{} has no string name", path.display()))?
            .to_owned();
        let phrases = match value.get("phrases") {
            None => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("{} has a non-string phrase", path.display()))
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => bail!("{} has a non-array phrases field", path.display()),
        };
        let phrases = phrases.join(&PHRASE_SEPARATOR.to_string());
        for field in [file.as_str(), name.as_str(), phrases.as_str()] {
            if field.contains(['\t', '\n', '\r']) {
                bail!(
                    "{} has a tab or line break in an indexed field",
                    path.display()
                );
            }
        }
        String::from_utf8(raw.clone())
            .with_context(|| format!("{} is not UTF-8", path.display()))?;
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&raw)?;
        let compressed = encoder.finish()?;
        let offset = body_data.len();
        let length = compressed.len();
        body_data.extend_from_slice(&compressed);
        let position = entries.len();
        let position_u16 = u16::try_from(position)
            .context("bundled manifest count exceeds static lookup capacity")?;
        names.push((name.to_owned(), position_u16));
        names.extend(
            phrases
                .split(PHRASE_SEPARATOR)
                .filter(|phrase| !phrase.is_empty())
                .map(|phrase| (phrase.to_owned(), position_u16)),
        );
        index.push_str(&format!(
            "{file}\t{}\t{name}\t{phrases}\t{offset}\t{length}\n",
            raw.len(),
        ));
        let hash = format!("{:x}", sha2::Sha256::digest(&raw));
        entries.push((file.clone(), raw.len(), hash, name, phrases, offset, length));
    }
    names.sort_by(|left, right| left.0.cmp(&right.0));
    let mut index_rust = String::from("static ENTRIES: [BundledEntry; ");
    index_rust.push_str(&entries.len().to_string());
    index_rust.push_str("] = [\n");
    for (file, bytes, hash, name, phrases, offset, length) in &entries {
        let phrase_list = phrases
            .split(PHRASE_SEPARATOR)
            .filter(|phrase| !phrase.is_empty())
            .map(|phrase| format!("{phrase:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        index_rust.push_str(&format!(
            "    BundledEntry {{\n        file: {file:?},\n        bytes: {bytes},\n        sha256: {hash:?},\n        name: {name:?},\n        phrases: &[{phrase_list}],\n        offset: {offset},\n        length: {length},\n        body: OnceLock::new(),\n    }},\n"
        ));
    }
    index_rust.push_str("];\nstatic NAMES: [(&str, u16); ");
    index_rust.push_str(&names.len().to_string());
    index_rust.push_str("] = [\n");
    for (name, position) in names {
        index_rust.push_str(&format!("    ({name:?}, {position}),\n"));
    }
    index_rust.push_str("];\n");
    Ok(Generated {
        index,
        index_rust,
        bodies: body_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    fn manifests_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
    }

    #[test]
    #[ignore = "manual bundled registry timing benchmark"]
    fn bundled_registry_timing_1000_iterations() {
        let dir = manifests_dir();
        let legacy_start = Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(parse_legacy_index_for_timing(INDEX_TSV));
        }
        eprintln!("1000 legacy TSV index parses: {:?}", legacy_start.elapsed());
        let static_start = Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(index().expect("static index"));
        }
        eprintln!("1000 static index accesses: {:?}", static_start.elapsed());
        let start = Instant::now();
        for _ in 0..1000 {
            Registry::load_default(&dir, crate::manifest::ManifestCheck::Lazy)
                .expect("load bundled registry")
                .resolve("cat")
                .expect("resolve cat");
        }
        eprintln!(
            "1000 bundled load_default + resolve iterations: {:?}",
            start.elapsed()
        );
    }

    fn parse_legacy_index_for_timing(text: &str) -> usize {
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(FORMAT_LINE));
        let mut entries = Vec::new();
        let mut names = std::collections::BTreeMap::new();
        for line in lines {
            let fields: Vec<&str> = line.split('\t').collect();
            let [file, _bytes, name, phrases, _offset, _length] = fields.as_slice() else {
                panic!("bad legacy index line");
            };
            let phrases: Vec<&str> = if phrases.is_empty() {
                Vec::new()
            } else {
                phrases.split(PHRASE_SEPARATOR).collect()
            };
            let position = entries.len();
            for key in std::iter::once(*name).chain(phrases.iter().copied()) {
                names.insert(key.to_owned(), position);
            }
            entries.push((*file, *name));
        }
        entries.len() + names.len()
    }

    /// The checked-in artifacts describe the manifests directory exactly: every file's
    /// size and name match, each body inflates to the file's bytes, and the lookup
    /// keys match the file's own name and phrases.
    #[test]
    fn checked_in_index_matches_the_manifests_directory() {
        let dir = manifests_dir();
        if std::env::var_os("EXECUTE_UPDATE_BUNDLED_INDEX").is_some() {
            let generated = generate(&dir).expect("generate bundled manifest index");
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bundled_manifests");
            fs::write(root.join("index.tsv"), generated.index).expect("write index");
            fs::write(root.join("index.rs"), generated.index_rust).expect("write Rust index");
            fs::write(root.join("bodies.bin"), generated.bodies).expect("write bodies");
            panic!("bundled manifest index rewritten; rerun without EXECUTE_UPDATE_BUNDLED_INDEX");
        }
        let generated = generate(&dir).expect("generate bundled manifest index");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bundled_manifests");
        assert_eq!(
            fs::read_to_string(root.join("index.tsv")).expect("read index"),
            generated.index,
            "bundled index is stale; run with EXECUTE_UPDATE_BUNDLED_INDEX=1"
        );
        assert_eq!(
            fs::read_to_string(root.join("index.rs")).expect("read Rust index"),
            generated.index_rust,
            "bundled Rust index is stale; run with EXECUTE_UPDATE_BUNDLED_INDEX=1"
        );
        assert_eq!(
            fs::read(root.join("bodies.bin")).expect("read bodies"),
            generated.bodies,
            "bundled bodies are stale; run with EXECUTE_UPDATE_BUNDLED_INDEX=1"
        );
        let index = index().expect("embedded index parses");
        assert!(
            directory_matches(&dir, index).expect("list manifests"),
            "bundled manifest index is stale; run with EXECUTE_UPDATE_BUNDLED_INDEX=1"
        );
        for (position, entry) in index.entries().iter().enumerate() {
            let source = fs::read_to_string(dir.join(entry.file())).expect("read manifest");
            assert_eq!(
                index.body(position).expect("inflate body"),
                source,
                "body of {} is stale",
                entry.file()
            );
            let value: serde_json::Value = serde_json::from_str(&source).expect("parse manifest");
            assert_eq!(
                value["name"].as_str(),
                Some(entry.name()),
                "{}",
                entry.file()
            );
            let phrases: Vec<&str> = value
                .get("phrases")
                .and_then(serde_json::Value::as_array)
                .map(|items| items.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            assert_eq!(phrases, entry.phrases(), "{}", entry.file());
        }
    }

    #[test]
    fn directory_check_rejects_an_extra_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        for entry in fs::read_dir(manifests_dir()).expect("read manifests") {
            let entry = entry.expect("entry");
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                fs::copy(entry.path(), dir.path().join(entry.file_name())).expect("copy");
            }
        }
        let index = index().expect("index");
        assert!(directory_matches(dir.path(), index).expect("check"));
        let shipped = dir.path().join("awk.json");
        let original = fs::read_to_string(&shipped).unwrap();
        let changed = original.replace("\"binary\": \"awk\"", "\"binary\": \"sed\"");
        assert_ne!(original, changed);
        fs::write(&shipped, changed).unwrap();
        fs::write(
            dir.path().join("extra.json"),
            r#"{"name":"custom.extra","binary":"printf","arguments":[]}"#,
        )
        .expect("extra");
        assert!(!directory_matches(dir.path(), index).expect("check"));
        assert!(Registry::load_default(dir.path(), crate::manifest::ManifestCheck::Lazy).is_err());
        assert!(Registry::load_default(dir.path(), crate::manifest::ManifestCheck::Full).is_err());
        assert!(Registry::load(dir.path()).is_ok());
        fs::remove_file(dir.path().join("extra.json")).expect("remove extra");
        fs::remove_file(dir.path().join("cat.json")).expect("remove indexed file");
        assert!(!directory_matches(dir.path(), index).expect("check missing"));
        assert!(Registry::load_default(dir.path(), crate::manifest::ManifestCheck::Lazy).is_err());
        assert!(Registry::load(dir.path()).is_ok());
    }

    #[test]
    fn startup_directory_check_does_not_query_file_metadata() {
        struct CountingDirectory {
            names: Vec<std::ffi::OsString>,
            metadata_calls: usize,
        }

        impl ManifestDirectoryProbe for CountingDirectory {
            type Entry = std::ffi::OsString;

            fn entries(&mut self, _root: &Path) -> io::Result<Vec<Self::Entry>> {
                Ok(self.names.clone())
            }

            fn name(&self, entry: &Self::Entry) -> std::ffi::OsString {
                entry.clone()
            }

            fn is_file(&mut self, _entry: &Self::Entry) -> io::Result<bool> {
                Ok(true)
            }

            fn metadata_len(&mut self, _entry: &Self::Entry) -> io::Result<u64> {
                self.metadata_calls += 1;
                Ok(0)
            }
        }

        let index = index().expect("index");
        let mut probe = CountingDirectory {
            names: index
                .entries()
                .iter()
                .map(|entry| entry.file().into())
                .collect(),
            metadata_calls: 0,
        };
        assert!(directory_matches_with(Path::new("."), index, &mut probe).expect("check"));
        assert_eq!(probe.metadata_calls, 0);
        let entry = probe.names[0].clone();
        let _ = probe.metadata_len(&entry).expect("count metadata call");
        assert_eq!(probe.metadata_calls, 1);
    }
}
