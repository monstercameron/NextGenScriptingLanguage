//! Parsed manifest registries kept between runs, so that a daemon serving runs with the same
//! `--manifest-dir` or `--manifest-catalog` parses those manifests once.
//!
//! An entry is immutable once built, keyed by the canonical path of its source, and checked
//! against a stamp of the source's files on every request. Each request, and each caller, gets
//! its own copy with fresh host state, so a policy installed for one run is never visible to
//! another. The daemon parent warms the cache before it forks, so each forked run inherits the
//! entries it needs.

use anyhow::Result;
use flow::manifest::Registry;
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Entries kept at once. When a new entry arrives beyond this, the least recently used goes.
const CAPACITY: usize = 8;

/// Where a registry was loaded from, by canonical path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Directory(PathBuf),
    Catalog(PathBuf),
}

/// File names, resolved origins and content digests for the source used by the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp(Vec<(OsString, PathBuf, [u8; 32])>);

struct Entry {
    source: Source,
    stamp: Stamp,
    registry: Registry,
}

thread_local! {
    // Least recently used first. Each process has its own copy: the daemon parent fills it, and
    // its forked runs inherit it. Nothing else touches it, so no lock is needed.
    static ENTRIES: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    // Off until the daemon parent enables it, so a cold run loads its manifests exactly as before.
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// Turns the cache on for this process. The daemon parent calls it; its forked runs inherit it.
pub fn enable() {
    ENABLED.with(|enabled| enabled.set(true));
}

/// The registry for `source`. With the cache on, a cached entry is used when the source's files
/// are unchanged; otherwise `build` loads it and the result is kept. Either way the caller gets a
/// copy with fresh host state. With the cache off, or when the source cannot be stamped, this is
/// `build` alone.
pub fn load(source: Source, build: impl FnOnce() -> Result<Registry>) -> Result<Registry> {
    if !ENABLED.with(Cell::get) {
        return build();
    }
    let Ok(current) = stamp(&source) else {
        return build();
    };
    if let Some(copy) = with_hit(&source, &current, Registry::detached) {
        return Ok(copy);
    }
    let built = build()?;
    if built.loaded_file_fingerprint() == Some(current.0.as_slice())
        && stamp(&source).is_ok_and(|after| after == current)
    {
        keep(source, current, built.detached());
    }
    Ok(built)
}

/// Loads `source` into the cache without returning it, for the daemon parent to call before a
/// fork. A failure is returned to the caller, which may ignore it: the run reports it itself.
pub fn warm(source: Source, build: impl FnOnce() -> Result<Registry>) -> Result<()> {
    enable();
    let Ok(current) = stamp(&source) else {
        return Ok(());
    };
    if with_hit(&source, &current, |_| ()).is_some() {
        return Ok(());
    }
    let built = build()?;
    if built.loaded_file_fingerprint() == Some(current.0.as_slice())
        && stamp(&source).is_ok_and(|after| after == current)
    {
        keep(source, current, built);
    }
    Ok(())
}

/// Hash each manifest in a directory, or the bytes of one catalog.
fn stamp(source: &Source) -> std::io::Result<Stamp> {
    match source {
        Source::Directory(path) => {
            let mut files = Vec::new();
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name();
                if Path::new(&name)
                    .extension()
                    .is_some_and(|ext| ext == "json")
                {
                    let path = entry.path();
                    files.push((name, fs::canonicalize(&path)?, digest(&path)?));
                }
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(Stamp(files))
        }
        Source::Catalog(path) => Ok(Stamp(vec![(
            path.file_name().unwrap_or_default().to_owned(),
            fs::canonicalize(path)?,
            digest(path)?,
        )])),
    }
}

// Stream content so the stamp is independent of modification times without buffering
// an entire manifest; never block on a FIFO masquerading as a JSON file.
fn digest(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "manifest is not a regular file",
        ));
    }
    flow::manifest::fingerprint_exact(&mut file, metadata.len())
}

/// Applies `use_entry` to the cached registry for `source` when its stamp still matches, and
/// marks it most recently used. A stale entry is dropped.
fn with_hit<R>(
    source: &Source,
    current: &Stamp,
    use_entry: impl FnOnce(&Registry) -> R,
) -> Option<R> {
    ENTRIES.with(|entries| {
        let mut entries = entries.borrow_mut();
        let index = entries.iter().position(|entry| &entry.source == source)?;
        if &entries[index].stamp != current {
            entries.remove(index);
            return None;
        }
        let entry = entries.remove(index);
        let result = use_entry(&entry.registry);
        entries.push(entry);
        Some(result)
    })
}

/// Keeps `registry` for `source`, replacing any entry for it, and evicts the least recently
/// used entries beyond `CAPACITY`.
fn keep(source: Source, stamp: Stamp, registry: Registry) {
    ENTRIES.with(|entries| {
        let mut entries = entries.borrow_mut();
        entries.retain(|entry| entry.source != source);
        entries.push(Entry {
            source,
            stamp,
            registry,
        });
        while entries.len() > CAPACITY {
            entries.remove(0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Restore only the modification time after replacing a fixture's contents.
    /// Windows requires FILE_WRITE_ATTRIBUTES on the handle used by set_times;
    /// File::open creates a read-only handle and fails with Access is denied.
    fn restore_modified_time(path: &Path, modified: std::time::SystemTime) {
        let mut options = fs::OpenOptions::new();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.access_mode(windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES);
        }
        #[cfg(not(windows))]
        options.read(true);
        options
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    /// Writes one manifest named `name` into `directory`, a copy of the shipped `cat` manifest
    /// with its name changed, since names must be unique within a registry.
    fn write_manifest(directory: &Path, name: &str) {
        let cat = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/cat.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(cat).unwrap()).unwrap();
        value["name"] = serde_json::Value::String(name.to_owned());
        value["phrases"] = serde_json::json!([format!("{name} copy")]);
        fs::write(
            directory.join(format!("{name}.json")),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }

    fn manifest_dir(names: &[&str]) -> tempfile::TempDir {
        enable();
        let directory = tempfile::tempdir().unwrap();
        for name in names {
            write_manifest(directory.path(), name);
        }
        directory
    }

    fn directory_source(directory: &tempfile::TempDir) -> Source {
        Source::Directory(fs::canonicalize(directory.path()).unwrap())
    }

    fn names(registry: &Registry) -> Vec<String> {
        let mut names: Vec<String> = registry
            .manifests()
            .unwrap()
            .iter()
            .map(|manifest| manifest.name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_unchanged_directory_is_served_from_the_cache() {
        let directory = manifest_dir(&["alpha"]);
        let source = directory_source(&directory);
        let mut builds = 0;
        for _ in 0..3 {
            let registry = load(source.clone(), || {
                builds += 1;
                Registry::load(directory.path())
            })
            .unwrap();
            assert_eq!(names(&registry), vec!["alpha".to_owned()]);
        }
        assert_eq!(builds, 1, "a stamp that still matches must not rebuild");
    }

    #[test]
    fn an_added_manifest_rebuilds_the_entry() {
        let directory = manifest_dir(&["alpha"]);
        let source = directory_source(&directory);
        load(source.clone(), || Registry::load(directory.path())).unwrap();
        write_manifest(directory.path(), "beta");
        let registry = load(source, || Registry::load(directory.path())).unwrap();
        assert_eq!(
            names(&registry),
            vec!["alpha".to_owned(), "beta".to_owned()]
        );
    }

    #[test]
    fn a_changed_size_rebuilds_the_entry() {
        let directory = manifest_dir(&["alpha"]);
        let source = directory_source(&directory);
        load(source.clone(), || Registry::load(directory.path())).unwrap();
        let path = directory.path().join("alpha.json");
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&" ".repeat(64));
        fs::write(&path, text).unwrap();
        let mut builds = 0;
        load(source, || {
            builds += 1;
            Registry::load(directory.path())
        })
        .unwrap();
        assert_eq!(builds, 1, "a changed size is a changed stamp");
    }

    #[test]
    fn same_size_edit_with_restored_mtime_rebuilds_the_entry() {
        let directory = manifest_dir(&["alpha"]);
        let source = directory_source(&directory);
        load(source.clone(), || Registry::load(directory.path())).unwrap();
        let path = directory.path().join("alpha.json");
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let changed = original.replace("alpha", "bravo");
        assert_ne!(changed, original);
        assert_eq!(changed.len(), original.len());
        fs::write(&path, changed).unwrap();
        restore_modified_time(&path, before);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        let result = load(source, || Registry::load(directory.path())).unwrap();
        assert_eq!(names(&result), vec!["bravo".to_owned()]);
    }

    #[test]
    fn same_size_catalog_edit_changes_stamp_even_when_mtime_is_restored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalog.json");
        fs::write(&path, b"{\"name\":\"alpha\"}").unwrap();
        let source = Source::Catalog(path.clone());
        let before = stamp(&source).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, b"{\"name\":\"bravo\"}").unwrap();
        restore_modified_time(&path, modified);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_ne!(before, stamp(&source).unwrap());
    }

    #[test]
    fn aba_changes_cannot_cache_registry_under_a_different_digest() {
        for catalog in [false, true] {
            for warming in [false, true] {
                let directory = manifest_dir(&[]);
                let path = directory.path().join(if catalog {
                    "catalog.json"
                } else {
                    "entry.json"
                });
                let manifest =
                    |name| format!(r#"{{"name":"{name}","binary":"printf","arguments":[]}}"#);
                let source_text = |name| {
                    if catalog {
                        format!(
                            r#"{{"format":"execute.manifest-catalog","version":1,"manifests":[{}]}}"#,
                            manifest(name)
                        )
                    } else {
                        manifest(name)
                    }
                };
                let a = source_text("alpha");
                let b = source_text("bravo");
                assert_eq!(a.len(), b.len());
                fs::write(&path, &a).unwrap();
                let source = if catalog {
                    Source::Catalog(path.clone())
                } else {
                    directory_source(&directory)
                };
                let build = || -> Result<Registry> {
                    fs::write(&path, &b)?;
                    let built = if catalog {
                        Registry::load_catalog(&path)?
                    } else {
                        Registry::load(directory.path())?
                    };
                    fs::write(&path, &a)?;
                    Ok(built)
                };
                if warming {
                    warm(source.clone(), build).unwrap();
                } else {
                    assert_eq!(names(&load(source.clone(), build).unwrap()), vec!["bravo"]);
                }
                let mut rebuilt = false;
                let current = load(source, || {
                    rebuilt = true;
                    if catalog {
                        Registry::load_catalog(&path)
                    } else {
                        Registry::load(directory.path())
                    }
                })
                .unwrap();
                assert!(
                    rebuilt,
                    "ABA registry must not be cached (catalog={catalog}, warm={warming})"
                );
                assert_eq!(names(&current), vec!["alpha"]);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_sources_fail_without_blocking_even_on_stamp_fallback() {
        use std::sync::mpsc;
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fifo.json");
        assert!(std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        for catalog in [false, true] {
            let path = path.clone();
            let dir = directory.path().to_path_buf();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                enable();
                let source = if catalog {
                    Source::Catalog(path.clone())
                } else {
                    Source::Directory(dir.clone())
                };
                warm(source.clone(), || {
                    if catalog {
                        Registry::load_catalog(&path)
                    } else {
                        Registry::load(&dir)
                    }
                })
                .unwrap();
                let result = load(source, || {
                    if catalog {
                        Registry::load_catalog(&path)
                    } else {
                        Registry::load(&dir)
                    }
                });
                tx.send(result.is_err()).unwrap();
            });
            assert!(rx
                .recv_timeout(Duration::from_secs(3))
                .expect("FIFO read blocked"));
        }
    }

    #[test]
    fn source_changed_during_build_is_not_cached() {
        let directory = manifest_dir(&["alpha"]);
        let source = directory_source(&directory);
        load(source.clone(), || {
            let registry = Registry::load(directory.path())?;
            write_manifest(directory.path(), "beta");
            Ok(registry)
        })
        .unwrap();
        let mut rebuilt = false;
        load(source, || {
            rebuilt = true;
            Registry::load(directory.path())
        })
        .unwrap();
        assert!(rebuilt);
    }

    #[test]
    fn the_least_recently_used_entry_is_evicted_first() {
        let directories: Vec<_> = (0..=CAPACITY).map(|_| manifest_dir(&["alpha"])).collect();
        let sources: Vec<Source> = directories.iter().map(directory_source).collect();
        for (directory, source) in directories.iter().zip(&sources) {
            load(source.clone(), || Registry::load(directory.path())).unwrap();
        }
        // The first one was least recently used and is gone; the last one is still cached.
        let mut rebuilt_first = false;
        load(sources[0].clone(), || {
            rebuilt_first = true;
            Registry::load(directories[0].path())
        })
        .unwrap();
        assert!(
            rebuilt_first,
            "the least recently used entry must be evicted"
        );
        let mut rebuilt_last = false;
        load(sources[CAPACITY].clone(), || {
            rebuilt_last = true;
            Registry::load(directories[CAPACITY].path())
        })
        .unwrap();
        assert!(!rebuilt_last, "a recent entry must stay cached");
    }

    #[test]
    fn a_cold_process_loads_every_time_and_keeps_nothing() {
        // A fresh thread has the cache off, as a cold run does.
        std::thread::spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            write_manifest(directory.path(), "alpha");
            let source = Source::Directory(fs::canonicalize(directory.path()).unwrap());
            let mut builds = 0;
            for _ in 0..2 {
                load(source.clone(), || {
                    builds += 1;
                    Registry::load(directory.path())
                })
                .unwrap();
            }
            assert_eq!(builds, 2);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn host_state_on_one_copy_is_not_seen_by_another() {
        #[derive(Default)]
        struct Marker(u32);
        let directory = manifest_dir(&["alpha"]);
        let source = directory_source(&directory);
        let first = load(source.clone(), || Registry::load(directory.path())).unwrap();
        first.set_host_state(Marker(7));
        let second = load(source, || Registry::load(directory.path())).unwrap();
        assert_eq!(second.host_state::<Marker>().0, 0);
        assert_eq!(first.host_state::<Marker>().0, 7);
    }
}
