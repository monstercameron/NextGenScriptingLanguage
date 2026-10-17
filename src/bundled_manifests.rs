//! The bundled manifest set, indexed at build time.
//!
//! `src/bundled_manifests/index.tsv` lists every bundled manifest with its file name,
//! byte size, canonical name, phrases, and where its body sits in `bodies.bin`. Each
//! body is a DEFLATE stream of the file's exact bytes, so inflating it reproduces the
//! source and the typed parse runs the same code as a file load. The index is
//! immutable and shared: every registry built from it reads the same static data, and
//! each manifest is parsed on first use.
//!
//! A registry uses the index only when the directory it was asked for holds exactly
//! the indexed files, with the same names and byte sizes. Any other directory takes
//! the full load, so errors for changed or broken manifests are reported as before.
//! The check is by name and size only: an edit that keeps a file's size is not seen.
//!
//! `generate` rebuilds both artifacts from a manifest directory after a full load has
//! validated every file. A test fails when the checked-in artifacts no longer match
//! the manifests; `EXECUTE_UPDATE_BUNDLED_INDEX=1 cargo test` rewrites them.

use crate::manifest::Registry;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::OnceLock;

const FORMAT_LINE: &str = "execute.bundled-manifests\t1";
const INDEX_TEXT: &str = include_str!("bundled_manifests/index.tsv");
const BODIES: &[u8] = include_bytes!("bundled_manifests/bodies.bin");
/// Separates phrases inside one index field. Unit separator: never in a manifest name.
const PHRASE_SEPARATOR: char = '\u{1f}';

/// One bundled manifest as the index describes it. `name` and `phrases` are the
/// resolution keys, so an applet resolves without inflating its body.
pub struct BundledEntry {
    file: &'static str,
    bytes: u64,
    name: &'static str,
    phrases: Vec<&'static str>,
    offset: usize,
    length: usize,
}

impl BundledEntry {
    pub fn file(&self) -> &'static str {
        self.file
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn phrases(&self) -> &[&'static str] {
        &self.phrases
    }
}

/// The immutable bundled index. Every registry built from it shares this one value.
/// `bodies` holds every manifest's exact source, concatenated and inflated once.
pub struct BundledIndex {
    entries: Vec<BundledEntry>,
    names: BTreeMap<String, usize>,
    bodies: String,
}

impl BundledIndex {
    pub fn entries(&self) -> &[BundledEntry] {
        &self.entries
    }

    /// Canonical names and phrases mapped to entry positions.
    pub fn names(&self) -> &BTreeMap<String, usize> {
        &self.names
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
        self.bodies
            .get(entry.offset..end)
            .ok_or_else(|| anyhow!("bundled manifest {} is outside the bodies", entry.file))
    }
}

static INDEX: OnceLock<std::result::Result<BundledIndex, String>> = OnceLock::new();

/// The embedded index and bodies, read once for the process.
pub fn index() -> Result<&'static BundledIndex> {
    INDEX
        .get_or_init(|| load_embedded().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|message| anyhow!("{message}"))
}

fn load_embedded() -> Result<BundledIndex> {
    let mut bodies = Vec::new();
    flate2::read::DeflateDecoder::new(BODIES)
        .read_to_end(&mut bodies)
        .context("inflating bundled manifest bodies")?;
    let bodies = String::from_utf8(bodies).context("bundled manifest bodies are not UTF-8")?;
    let mut index = parse_index(INDEX_TEXT)?;
    index.bodies = bodies;
    Ok(index)
}

fn parse_index(text: &'static str) -> Result<BundledIndex> {
    let mut lines = text.lines();
    let header = lines.next().context("bundled manifest index is empty")?;
    if header != FORMAT_LINE {
        bail!("unsupported bundled manifest index header {header:?}");
    }
    let mut entries = Vec::new();
    let mut names = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&'static str> = line.split('\t').collect();
        let [file, bytes, name, phrases, offset, length] = fields.as_slice() else {
            bail!(
                "bundled manifest index line has {} fields, expected 6",
                fields.len()
            );
        };
        let phrases: Vec<&'static str> = if phrases.is_empty() {
            Vec::new()
        } else {
            phrases.split(PHRASE_SEPARATOR).collect()
        };
        let index = entries.len();
        for key in std::iter::once(*name).chain(phrases.iter().copied()) {
            if names.insert(key.to_owned(), index).is_some() {
                bail!("bundled manifest index repeats the name or phrase {key:?}");
            }
        }
        entries.push(BundledEntry {
            file,
            bytes: bytes.parse().context("bundled manifest size")?,
            name,
            phrases,
            offset: offset.parse().context("bundled manifest offset")?,
            length: length.parse().context("bundled manifest length")?,
        });
    }
    Ok(BundledIndex {
        entries,
        names,
        bodies: String::new(),
    })
}

/// Whether `root` holds exactly the indexed files, by name and byte size. An
/// unreadable directory, a subdirectory or symlink among the JSON files, or any
/// difference is a mismatch, and the caller falls back to the full load.
pub fn directory_matches(root: &Path, index: &BundledIndex) -> io::Result<bool> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            return Ok(false);
        }
        let Ok(name) = entry.file_name().into_string() else {
            return Ok(false);
        };
        found.push((name, entry.metadata()?.len()));
    }
    found.sort();
    Ok(found.len() == index.entries.len()
        && found
            .iter()
            .zip(&index.entries)
            .all(|((name, bytes), entry)| name == entry.file && *bytes == entry.bytes))
}

/// Both artifacts generated from one manifest directory.
pub struct Generated {
    pub index: String,
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
    let mut bodies = String::new();
    for (file, path) in &files {
        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("{} has no string name", path.display()))?;
        let phrases = match value.get("phrases") {
            None => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .ok_or_else(|| anyhow!("{} has a non-string phrase", path.display()))
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => bail!("{} has a non-array phrases field", path.display()),
        };
        let phrases = phrases.join(&PHRASE_SEPARATOR.to_string());
        for field in [file.as_str(), name, phrases.as_str()] {
            if field.contains(['\t', '\n', '\r']) {
                bail!(
                    "{} has a tab or line break in an indexed field",
                    path.display()
                );
            }
        }
        let source = String::from_utf8(raw.clone())
            .with_context(|| format!("{} is not UTF-8", path.display()))?;
        let offset = bodies.len();
        bodies.push_str(&source);
        index.push_str(&format!(
            "{file}\t{}\t{name}\t{phrases}\t{offset}\t{}\n",
            raw.len(),
            raw.len()
        ));
    }
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bodies.as_bytes())?;
    let compressed = encoder.finish()?;
    Ok(Generated {
        index,
        bodies: compressed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn manifests_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
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
            fs::write(root.join("bodies.bin"), generated.bodies).expect("write bodies");
            panic!("bundled manifest index rewritten; rerun without EXECUTE_UPDATE_BUNDLED_INDEX");
        }
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
        fs::write(dir.path().join("extra.json"), "{}").expect("extra");
        assert!(!directory_matches(dir.path(), index).expect("check"));
    }
}
