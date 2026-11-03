//! `fs.stat`, `fs.list` and `fs.find`: typed file metadata and discovery.
//!
//! Scripts ask about files beyond "does it exist": how old is this lock, who
//! owns it and with what mode, which file in this directory is newest, how
//! big is it. These applets answer with JSON records instead of `stat`,
//! `find` or `ls` child text, so `list.sort`, `list.top` and `json.filter`
//! apply without text parsing. All three are read-only under the
//! `filesystem.read` capability and effect, like `fs.glob` and `fs.test`.
//!
//! Every record carries `path`, `name`, `exists`, `kind`, `size`,
//! millisecond `mtime_ms`/`atime_ms`/`ctime_ms`, `mode`, `uid`, `gid`,
//! `nlink`, `symlink_target`, `device` and `inode`. An absent path answers
//! `exists: false` with nulls rather than failing, mirroring `fs.test`;
//! other stat failures (for example permission denied searching a parent
//! directory) are errors. `kind` is one of `file`, `directory`, `symlink`,
//! `fifo`, `socket` or `other`.
//!
//! Platform mapping: `mode` is the Unix permission bits and `uid`/`gid`,
//! `nlink`, `device` and `inode` are the Unix identity numbers; all are null
//! on Windows, which has no equivalent. `ctime_ms` is the status-change time
//! on Unix and the creation time on Windows, and is null when the clock is
//! unavailable. `atime_ms` is null when the filesystem does not report it.
//! Socket and FIFO kinds are detected on Unix only; Windows reports such
//! entries as `other` or `file`/`directory`.
//!
//! `fs.stat` stats one path. `follow_symlinks` (default true) selects target
//! metadata like GNU `stat`; with false the link itself is reported and a
//! dangling link answers `exists: false` with its `kind` and target kept.
//!
//! `fs.list` lists one directory without following final symlinks and sorts
//! the records by a named key. `fs.find` walks a tree without ever
//! descending through a symlinked directory; `follow_symlinks` only selects
//! record metadata. Both stop at an entry and result budget like `fs.glob`
//! (more than 1,000,000 examined entries or 100,000 results fail), honour
//! the cooperative execution budget for cancellation and deadlines, skip
//! already-visited directories so bind or junction cycles terminate, and
//! return records sorted by path bytes for deterministic output.

use super::filesystem::resolve_path;
use super::glob::compile_name_pattern;
use super::glob::NamePattern;
use super::{
    checkpoint_work, failed, invalid, io_error, optional_bool, optional_u64, required_string,
    AppletError, ExecutionContext,
};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ENTRY_KINDS: &[&str] = &["file", "directory", "symlink", "fifo", "socket", "other"];
const SORT_KEYS: &[&str] = &["name", "kind", "size", "mtime_ms"];
const MAX_FIND_RESULTS: u64 = 100_000;
const MAX_FIND_ENTRIES: usize = 1_000_000;
const MAX_FIND_DEPTH: u64 = 256;
const MAX_NAME_PATTERNS: usize = 32;

pub(super) fn stat(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = required_string(args, "path")?;
    check_path_text(text)?;
    let follow = optional_bool(args, "follow_symlinks", true)?;
    let fs_path = resolve_path(text, cwd);
    context.checkpoint()?;
    record(text, &fs_path, follow, context)
}

pub(super) fn list(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = required_string(args, "path")?;
    check_path_text(text)?;
    let include_hidden = optional_bool(args, "include_hidden", false)?;
    let kinds = parse_kinds(args)?;
    let sort = parse_sort(args)?;
    let descending = optional_bool(args, "descending", false)?;
    let fs_path = resolve_path(text, cwd);
    context.checkpoint()?;
    let mut entries: Vec<(String, Value)> = Vec::new();
    for (name, fs_child) in read_names(&fs_path)? {
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let display = join_display(text, &name);
        let entry = record(&display, &fs_child, false, context)?;
        if kinds.as_ref().is_some_and(|kinds| {
            entry["kind"]
                .as_str()
                .is_none_or(|kind| !kinds.contains(kind))
        }) {
            continue;
        }
        entries.push((display, entry));
    }
    sort_entries(&mut entries, &sort, descending);
    Ok(Value::Array(
        entries.into_iter().map(|(_, entry)| entry).collect(),
    ))
}

pub(super) fn find(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = required_string(args, "path")?;
    check_path_text(text)?;
    let query = FindQuery::parse(args)?;
    let fs_path = resolve_path(text, cwd);
    context.checkpoint()?;
    // An absent root is an empty match, mirroring `fs.glob`; a non-directory
    // root is one depth-zero candidate.
    let root_metadata = match fs::symlink_metadata(&fs_path) {
        Ok(metadata) => metadata,
        Err(error) if is_absent(&error) => return Ok(Value::Array(Vec::new())),
        Err(error) => {
            return Err(io_error(error, format!("finding {}", fs_path.display())));
        }
    };
    let mut walk = FindWalk {
        query: &query,
        include_hidden: optional_bool(args, "include_hidden", false)?,
        context,
        results: Vec::new(),
        examined: 0,
        visited: HashSet::new(),
    };
    if root_metadata.is_dir() {
        walk.mark_visited(&fs_path)?;
        let root_name = entry_name(&fs_path, text)?;
        if query.depth_contains(0) {
            let entry = record(text, &fs_path, query.follow, context)?;
            if walk.keeps(&root_name, &entry)? {
                walk.push(text.to_owned(), entry)?;
            }
        }
        walk.walk(text, &fs_path, 0)?;
    } else {
        let name = entry_name(&fs_path, text)?;
        if query.depth_contains(0) {
            let entry = record(text, &fs_path, query.follow, context)?;
            if walk.keeps(&name, &entry)? {
                walk.push(text.to_owned(), entry)?;
            }
        }
    }
    let mut results = walk.results;
    results.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(Value::Array(
        results.into_iter().map(|(_, entry)| entry).collect(),
    ))
}

fn check_path_text(text: &str) -> Result<(), AppletError> {
    if text.is_empty() || text.contains('\0') {
        return Err(invalid("args.path must be nonempty and contain no NUL"));
    }
    Ok(())
}

fn is_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Build one metadata record. `display` is the path text to report (the
/// input form, so relative stays relative); `fs_path` is what syscalls use.
fn record(
    display: &str,
    fs_path: &Path,
    follow: bool,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let link_metadata = match fs::symlink_metadata(fs_path) {
        Ok(metadata) => metadata,
        Err(error) if is_absent(&error) => return Ok(absent_record(display)),
        Err(error) => {
            return Err(io_error(error, format!("stating {}", fs_path.display())));
        }
    };
    let is_link = link_metadata.file_type().is_symlink();
    let symlink_target = if is_link {
        Some(
            fs::read_link(fs_path)
                .map_err(|error| io_error(error, format!("reading symlink {}", fs_path.display())))?
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };
    let metadata = if follow {
        match fs::metadata(fs_path) {
            Ok(metadata) => Some(metadata),
            Err(error) if is_absent(&error) => None,
            Err(error) => {
                return Err(io_error(error, format!("stating {}", fs_path.display())));
            }
        }
    } else {
        Some(link_metadata)
    };
    let Some(metadata) = metadata else {
        // Absent, or a dangling link when following: keep the link identity
        // like `fs.test` keeps `symlink` true while `exists` is false.
        return Ok(dangling_record(display, symlink_target));
    };
    let kind = kind_of(&metadata, is_link, follow);
    let mtime_ms = metadata
        .modified()
        .map_err(|error| io_error(error, format!("stating {}", fs_path.display())))
        .and_then(system_time_ms)?;
    let atime_ms = metadata.accessed().ok().map(system_time_ms).transpose()?;
    let ctime_ms = change_time_ms(&metadata)?;
    Ok(serde_json::json!({
        "path": display,
        "name": entry_name(fs_path, display)?,
        "exists": true,
        "kind": kind,
        "size": metadata.len(),
        "mtime_ms": mtime_ms,
        "atime_ms": atime_ms,
        "ctime_ms": ctime_ms,
        "mode": file_mode(&metadata),
        "uid": file_uid(&metadata),
        "gid": file_gid(&metadata),
        "nlink": file_nlink(&metadata),
        "symlink_target": symlink_target,
        "device": file_device(&metadata),
        "inode": file_inode(&metadata),
    }))
}

fn absent_record(display: &str) -> Value {
    serde_json::json!({
        "path": display,
        "name": Path::new(display).file_name().and_then(|name| name.to_str()).unwrap_or(display),
        "exists": false,
        "kind": Value::Null,
        "size": Value::Null,
        "mtime_ms": Value::Null,
        "atime_ms": Value::Null,
        "ctime_ms": Value::Null,
        "mode": Value::Null,
        "uid": Value::Null,
        "gid": Value::Null,
        "nlink": Value::Null,
        "symlink_target": Value::Null,
        "device": Value::Null,
        "inode": Value::Null,
    })
}

fn dangling_record(display: &str, symlink_target: Option<String>) -> Value {
    let mut entry = absent_record(display);
    entry["kind"] = Value::String("symlink".into());
    entry["symlink_target"] = symlink_target.map_or(Value::Null, Value::String);
    entry
}

fn kind_of(metadata: &fs::Metadata, is_link: bool, follow: bool) -> &'static str {
    let file_type = metadata.file_type();
    if !follow && is_link {
        return "symlink";
    }
    if file_type.is_file() {
        "file"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symlink"
    } else if is_fifo(&file_type) {
        "fifo"
    } else if is_socket(&file_type) {
        "socket"
    } else {
        "other"
    }
}

#[cfg(unix)]
fn is_fifo(file_type: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_fifo()
}

#[cfg(not(unix))]
fn is_fifo(_file_type: &fs::FileType) -> bool {
    false
}

#[cfg(unix)]
fn is_socket(file_type: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_socket()
}

#[cfg(not(unix))]
fn is_socket(_file_type: &fs::FileType) -> bool {
    false
}

fn system_time_ms(time: SystemTime) -> Result<i64, AppletError> {
    let range = || invalid("timestamp is outside the supported range");
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).map_err(|_| range()),
        Err(before) => i64::try_from(before.duration().as_millis())
            .map_err(|_| range())?
            .checked_neg()
            .ok_or_else(range),
    }
}

/// Status-change time on Unix, creation time on Windows, else null.
fn change_time_ms(metadata: &fs::Metadata) -> Result<Option<i64>, AppletError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ms =
            i128::from(metadata.ctime()) * 1000 + i128::from(metadata.ctime_nsec()) / 1_000_000;
        i64::try_from(ms)
            .map(Some)
            .map_err(|_| invalid("timestamp is outside the supported range"))
    }
    #[cfg(not(unix))]
    {
        metadata.created().ok().map(system_time_ms).transpose()
    }
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn file_uid(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(u64::from(metadata.uid()))
}

#[cfg(not(unix))]
fn file_uid(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn file_gid(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(u64::from(metadata.gid()))
}

#[cfg(not(unix))]
fn file_gid(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn file_nlink(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.nlink())
}

#[cfg(not(unix))]
fn file_nlink(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn file_device(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.dev())
}

#[cfg(not(unix))]
fn file_device(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn file_inode(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn file_inode(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

/// The file name for a record. Inputs are valid UTF-8, so `fs.stat` always
/// renders; directory children are checked when read.
fn entry_name(fs_path: &Path, display: &str) -> Result<String, AppletError> {
    fs_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .or_else(|| {
            Path::new(display)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            failed(
                "io_error",
                format!("entry name is not valid UTF-8: {}", fs_path.display()),
            )
        })
}

/// Join a child name onto the input path text so relative stays relative,
/// mirroring `fs.glob` display paths.
fn join_display(parent: &str, name: &str) -> String {
    Path::new(parent).join(name).to_string_lossy().into_owned()
}

/// Read one directory's entry names with their filesystem paths. An absent
/// directory is an empty match like `fs.glob`; a non-directory is an error.
fn read_names(dir: &Path) -> Result<Vec<(String, PathBuf)>, AppletError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
            return Err(invalid(format!(
                "path is not a directory: {}",
                dir.display()
            )));
        }
        Err(error) => return Err(io_error(error, format!("reading {}", dir.display()))),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error(error, format!("reading {}", dir.display())))?;
        let name = entry.file_name();
        let Some(text) = name.to_str() else {
            return Err(failed(
                "io_error",
                format!("found a non-UTF-8 name in {}", dir.display()),
            ));
        };
        names.push((text.to_owned(), entry.path()));
    }
    Ok(names)
}

fn parse_kinds(args: &Map<String, Value>) -> Result<Option<HashSet<String>>, AppletError> {
    match args.get("kinds") {
        None => Ok(None),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err(invalid("args.kinds must not be empty"));
            }
            let mut kinds = HashSet::new();
            for item in items {
                let Some(kind) = item.as_str() else {
                    return Err(invalid("args.kinds must be a list of strings"));
                };
                if !ENTRY_KINDS.contains(&kind) {
                    return Err(invalid(format!(
                        "args.kinds must be any of {}",
                        ENTRY_KINDS.join(", ")
                    )));
                }
                kinds.insert(kind.to_owned());
            }
            Ok(Some(kinds))
        }
        Some(_) => Err(invalid("args.kinds must be a list of strings")),
    }
}

fn parse_sort(args: &Map<String, Value>) -> Result<String, AppletError> {
    match args.get("sort") {
        None => Ok("name".into()),
        Some(Value::String(sort)) if SORT_KEYS.contains(&sort.as_str()) => Ok(sort.clone()),
        Some(Value::String(_)) => Err(invalid(format!(
            "args.sort must be one of {}",
            SORT_KEYS.join(", ")
        ))),
        Some(_) => Err(invalid(format!(
            "args.sort must be one of {}",
            SORT_KEYS.join(", ")
        ))),
    }
}

fn sort_entries(entries: &mut [(String, Value)], sort: &str, descending: bool) {
    entries.sort_by(|left, right| {
        let ordering = match sort {
            "kind" => cmp_str(&left.1["kind"], &right.1["kind"]),
            "size" => cmp_u64(&left.1["size"], &right.1["size"]),
            "mtime_ms" => cmp_i64(&left.1["mtime_ms"], &right.1["mtime_ms"]),
            _ => left.1["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(right.1["name"].as_str().unwrap_or_default()),
        }
        .then_with(|| left.0.cmp(&right.0));
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
}

/// Nulls sort last so partial records never outrank complete ones.
fn cmp_str(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left.as_str(), right.as_str()) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn cmp_u64(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left.as_u64(), right.as_u64()) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn cmp_i64(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left.as_i64(), right.as_i64()) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn optional_i64(args: &Map<String, Value>, name: &str) -> Result<Option<i64>, AppletError> {
    match args.get(name) {
        None => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
            .map(Some)
            .ok_or_else(|| invalid(format!("args.{name} must be an integer"))),
        Some(_) => Err(invalid(format!("args.{name} must be an integer"))),
    }
}

fn optional_size(args: &Map<String, Value>, name: &str) -> Result<Option<u64>, AppletError> {
    match args.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(format!("args.{name} must be a nonnegative integer"))),
    }
}

struct FindQuery {
    kinds: Option<HashSet<String>>,
    names: Vec<NamePattern>,
    min_depth: u64,
    max_depth: u64,
    newer_than: Option<i64>,
    older_than: Option<i64>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    follow: bool,
    limit: u64,
}

impl FindQuery {
    fn parse(args: &Map<String, Value>) -> Result<Self, AppletError> {
        let kinds = parse_kinds(args)?;
        let names = match args.get("names") {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                if items.is_empty() {
                    return Err(invalid("args.names must not be empty"));
                }
                if items.len() > MAX_NAME_PATTERNS {
                    return Err(invalid(format!(
                        "args.names holds more than {MAX_NAME_PATTERNS} patterns"
                    )));
                }
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .ok_or_else(|| invalid("args.names must be a list of strings"))
                            .and_then(compile_name_pattern)
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
            Some(_) => return Err(invalid("args.names must be a list of strings")),
        };
        let min_depth = optional_u64(args, "min_depth", 0)?;
        let max_depth = optional_u64(args, "max_depth", MAX_FIND_DEPTH)?;
        if max_depth > MAX_FIND_DEPTH {
            return Err(invalid(format!(
                "args.max_depth exceeds the {MAX_FIND_DEPTH}-level limit"
            )));
        }
        if min_depth > max_depth {
            return Err(invalid("args.min_depth exceeds args.max_depth"));
        }
        let newer_than = optional_i64(args, "newer_than")?;
        let older_than = optional_i64(args, "older_than")?;
        let min_size = optional_size(args, "min_size")?;
        let max_size = optional_size(args, "max_size")?;
        if min_size.is_some_and(|min| max_size.is_some_and(|max| min > max)) {
            return Err(invalid("args.min_size exceeds args.max_size"));
        }
        let follow = optional_bool(args, "follow_symlinks", false)?;
        let limit = optional_u64(args, "limit", MAX_FIND_RESULTS)?;
        if limit == 0 {
            return Err(invalid("args.limit must be a positive integer"));
        }
        if limit > MAX_FIND_RESULTS {
            return Err(invalid(format!(
                "args.limit exceeds the {MAX_FIND_RESULTS}-entry limit"
            )));
        }
        Ok(Self {
            kinds,
            names,
            min_depth,
            max_depth,
            newer_than,
            older_than,
            min_size,
            max_size,
            follow,
            limit,
        })
    }

    fn depth_contains(&self, depth: u64) -> bool {
        depth >= self.min_depth && depth <= self.max_depth
    }
}

struct FindWalk<'query, 'context, 'cancel> {
    query: &'query FindQuery,
    include_hidden: bool,
    context: &'context ExecutionContext<'cancel>,
    results: Vec<(String, Value)>,
    examined: usize,
    visited: HashSet<PathBuf>,
}

impl FindWalk<'_, '_, '_> {
    fn walk(&mut self, display: &str, fs_dir: &Path, depth: u64) -> Result<bool, AppletError> {
        for (name, fs_child) in self.read_names(fs_dir)? {
            // Hidden entries are skipped unless `include_hidden` or a
            // leading-dot pattern opts in, mirroring `fs.glob`.
            if !self.include_hidden && name.starts_with('.') && !self.hidden_opt_in(&name) {
                continue;
            }
            let child_depth = depth + 1;
            let child_display = join_display(display, &name);
            let entry = record(&child_display, &fs_child, self.query.follow, self.context)?;
            if self.query.depth_contains(child_depth)
                && self.keeps(&name, &entry)?
                && self.push(child_display.clone(), entry)?
            {
                return Ok(true);
            }
            if child_depth < self.query.max_depth && self.is_real_directory(&fs_child)? {
                if !self.mark_visited(&fs_child)? {
                    continue;
                }
                if self.walk(&child_display, &fs_child, child_depth)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Read one directory, counting entries against the examined budget.
    /// `NotADirectory` cannot happen here (callers check first) and is still
    /// reported rather than silently skipped.
    fn read_names(&mut self, fs_dir: &Path) -> Result<Vec<(String, PathBuf)>, AppletError> {
        let entries = match fs::read_dir(fs_dir) {
            Ok(entries) => entries,
            Err(error) => return Err(io_error(error, format!("reading {}", fs_dir.display()))),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| io_error(error, format!("reading {}", fs_dir.display())))?;
            self.examined += 1;
            if self.examined > MAX_FIND_ENTRIES {
                return Err(failed(
                    "find_limit",
                    format!("fs.find examined more than {MAX_FIND_ENTRIES} directory entries"),
                ));
            }
            checkpoint_work(self.context, self.examined)?;
            let name = entry.file_name();
            let Some(text) = name.to_str() else {
                return Err(failed(
                    "io_error",
                    format!("fs.find found a non-UTF-8 name in {}", fs_dir.display()),
                ));
            };
            names.push((text.to_owned(), entry.path()));
        }
        Ok(names)
    }

    fn name_matches(&self, name: &str) -> bool {
        self.query.names.is_empty()
            || self
                .query
                .names
                .iter()
                .any(|pattern| pattern.matches(name, self.include_hidden))
    }

    /// A hidden name is visible only through a leading-dot pattern; with no
    /// patterns only `include_hidden` (checked by the caller) opts it in.
    fn hidden_opt_in(&self, name: &str) -> bool {
        self.query
            .names
            .iter()
            .any(|pattern| pattern.matches(name, false))
    }

    /// `name` is the entry file name for `names` filtering, applied to the
    /// root itself as well as every descendant like GNU `find`.
    fn keeps(&self, name: &str, entry: &Value) -> Result<bool, AppletError> {
        if self.query.kinds.as_ref().is_some_and(|kinds| {
            entry["kind"]
                .as_str()
                .is_none_or(|kind| !kinds.contains(kind))
        }) {
            return Ok(false);
        }
        if !name.is_empty() && !self.name_matches(name) {
            return Ok(false);
        }
        if self.query.newer_than.is_some_and(|bound| {
            entry["mtime_ms"]
                .as_i64()
                .is_none_or(|mtime| mtime <= bound)
        }) {
            return Ok(false);
        }
        if self.query.older_than.is_some_and(|bound| {
            entry["mtime_ms"]
                .as_i64()
                .is_none_or(|mtime| mtime >= bound)
        }) {
            return Ok(false);
        }
        if self
            .query
            .min_size
            .is_some_and(|bound| entry["size"].as_u64().is_none_or(|size| size < bound))
        {
            return Ok(false);
        }
        if self
            .query
            .max_size
            .is_some_and(|bound| entry["size"].as_u64().is_none_or(|size| size > bound))
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// Record one match. `Ok(true)` stops the walk: an explicit `limit`
    /// below the result cap truncates, while reaching the cap fails so a
    /// bounded query never silently drops matches.
    fn push(&mut self, display: String, entry: Value) -> Result<bool, AppletError> {
        if self.results.len() as u64 >= self.query.limit {
            if self.query.limit >= MAX_FIND_RESULTS {
                return Err(failed(
                    "find_limit",
                    format!(
                        "fs.find matched more than {MAX_FIND_RESULTS} entries; narrow names, kinds, or depth"
                    ),
                ));
            }
            return Ok(true);
        }
        self.results.push((display, entry));
        Ok(false)
    }

    fn is_real_directory(&self, path: &Path) -> Result<bool, AppletError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if is_absent(&error) => Ok(false),
            Err(error) => Err(io_error(error, format!("stating {}", path.display()))),
        }
    }

    /// Skip already-visited directories so bind or junction cycles terminate.
    /// Returns false when the directory was visited before.
    fn mark_visited(&mut self, path: &Path) -> Result<bool, AppletError> {
        let canonical = fs::canonicalize(path)
            .map_err(|error| io_error(error, format!("resolving {}", path.display())))?;
        Ok(self.visited.insert(canonical))
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::applets::ExecutionBudgetExceeded;
    use serde_json::json;

    fn run(operation: &str, args: Value, cwd: &Path) -> Result<Value, AppletError> {
        let context = ExecutionContext::unrestricted();
        match operation {
            "fs.stat" => stat(args.as_object().unwrap(), cwd, &context),
            "fs.list" => list(args.as_object().unwrap(), cwd, &context),
            "fs.find" => find(args.as_object().unwrap(), cwd, &context),
            _ => panic!("unknown operation {operation}"),
        }
    }

    fn stat_path(path: &Path, extra: Value) -> Result<Value, AppletError> {
        let mut arguments = json!({"path": path.to_str().unwrap()});
        for (key, value) in extra.as_object().unwrap() {
            arguments[key] = value.clone();
        }
        run("fs.stat", arguments, Path::new("/"))
    }

    #[test]
    fn stat_reports_typed_fields_for_a_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("lock");
        fs::write(&file, b"0123456789").unwrap();
        let entry = stat_path(&file, json!({})).unwrap();
        assert_eq!(entry["exists"], json!(true));
        assert_eq!(entry["kind"], json!("file"));
        assert_eq!(entry["size"], json!(10));
        assert_eq!(entry["path"], json!(file.to_str().unwrap()));
        assert_eq!(entry["name"], json!("lock"));
        assert!(entry["mtime_ms"].as_i64().unwrap() > 0);
        assert_eq!(entry["symlink_target"], Value::Null);
        #[cfg(unix)]
        {
            assert!(entry["mode"].as_u64().is_some());
            assert!(entry["uid"].as_u64().is_some());
            assert!(entry["gid"].as_u64().is_some());
            assert!(entry["nlink"].as_u64().is_some());
            assert!(entry["device"].as_u64().is_some());
            assert!(entry["inode"].as_u64().is_some());
            // Repeat with explicit follow_symlinks for the stale-lock shape.
            let followed = stat_path(&file, json!({"follow_symlinks": true})).unwrap();
            assert_eq!(followed["kind"], json!("file"));
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn stat_returns_documented_nulls_on_windows() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("lock");
        fs::write(&file, b"data").unwrap();
        let entry = stat_path(&file, json!({})).unwrap();
        assert_eq!(entry["exists"], json!(true));
        assert_eq!(entry["kind"], json!("file"));
        assert_eq!(entry["size"], json!(4));
        for field in ["mode", "uid", "gid", "nlink", "device", "inode"] {
            assert_eq!(entry[field], Value::Null, "{field}");
        }
        assert!(entry["mtime_ms"].as_i64().is_some());
    }

    #[test]
    fn stat_reports_absence_without_failing() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let entry = stat_path(&missing, json!({})).unwrap();
        assert_eq!(entry["exists"], json!(false));
        for field in [
            "kind",
            "size",
            "mtime_ms",
            "atime_ms",
            "ctime_ms",
            "mode",
            "uid",
            "gid",
            "nlink",
            "symlink_target",
            "device",
            "inode",
        ] {
            assert_eq!(entry[field], Value::Null, "{field}");
        }
        assert!(stat_path(Path::new(""), json!({})).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stat_follow_symlinks_selects_link_or_target() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, b"xy").unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        let followed = stat_path(&link, json!({})).unwrap();
        assert_eq!(followed["kind"], json!("file"));
        assert_eq!(followed["size"], json!(2));
        assert_eq!(followed["symlink_target"], json!(target.to_str().unwrap()));
        let link_only = stat_path(&link, json!({"follow_symlinks": false})).unwrap();
        assert_eq!(link_only["kind"], json!("symlink"));
        assert_eq!(link_only["symlink_target"], json!(target.to_str().unwrap()));
        let dangling = temp.path().join("dangling");
        symlink(temp.path().join("nowhere"), &dangling).unwrap();
        let dangling_followed = stat_path(&dangling, json!({})).unwrap();
        assert_eq!(dangling_followed["exists"], json!(false));
        assert_eq!(dangling_followed["kind"], json!("symlink"));
    }

    #[test]
    fn list_sorts_filters_and_hides_dotfiles_by_default() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("b.txt"), b"bb").unwrap();
        fs::write(temp.path().join("a.txt"), b"a").unwrap();
        fs::write(temp.path().join(".hidden"), b"h").unwrap();
        fs::create_dir(temp.path().join("sub")).unwrap();
        let root = temp.path().to_str().unwrap();
        let entries = run("fs.list", json!({"path": root}), temp.path()).unwrap();
        let names: Vec<_> = entries
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
        let by_size = run(
            "fs.list",
            json!({"path": root, "sort": "size", "descending": true, "kinds": ["file"]}),
            temp.path(),
        )
        .unwrap();
        let sizes: Vec<_> = by_size
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(sizes, vec!["b.txt", "a.txt"]);
        let files_only = run(
            "fs.list",
            json!({"path": root, "kinds": ["file"], "include_hidden": true}),
            temp.path(),
        )
        .unwrap();
        assert_eq!(files_only.as_array().unwrap().len(), 3);
        let bad_sort = run(
            "fs.list",
            json!({"path": root, "sort": "owner"}),
            temp.path(),
        );
        assert!(bad_sort.is_err());
        let not_a_dir = run(
            "fs.list",
            json!({"path": temp.path().join("a.txt").to_str().unwrap()}),
            temp.path(),
        );
        assert!(not_a_dir.is_err());
    }

    #[test]
    fn find_matches_names_depths_sizes_and_times() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("old.log"), b"0123456789").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("new.json"), b"{}").unwrap();
        // Pin every mtime after creating the tree so directory mtimes from
        // the setup itself cannot leak into the time filters.
        set_mtime(&root.join("old.log"), 1_700_000_000);
        set_mtime(&root.join("sub").join("new.json"), 1_800_000_000);
        set_mtime(&root.join("sub"), 1_600_000_000);
        set_mtime(&root, 1_600_000_000);
        let root_text = root.to_str().unwrap();
        let names: Vec<String> = find_names(
            &run(
                "fs.find",
                json!({"path": root_text, "names": ["*.log"], "min_depth": 1}),
                temp.path(),
            )
            .unwrap(),
        );
        assert_eq!(names, vec![join_display(root_text, "old.log")]);
        let cutoff = 1_750_000_000i64 * 1000;
        let fresh: Vec<String> = find_names(
            &run(
                "fs.find",
                json!({"path": root_text, "newer_than": cutoff, "min_depth": 1}),
                temp.path(),
            )
            .unwrap(),
        );
        assert_eq!(
            fresh,
            vec![join_display(&join_display(root_text, "sub"), "new.json")]
        );
        let shallow: Vec<String> = find_names(
            &run(
                "fs.find",
                json!({"path": root_text, "max_depth": 1}),
                temp.path(),
            )
            .unwrap(),
        );
        assert!(shallow.contains(&root_text.to_owned()));
        assert!(shallow.contains(&join_display(root_text, "old.log")));
        assert!(!shallow.iter().any(|path| path.ends_with("new.json")));
        let tiny = run(
            "fs.find",
            json!({"path": root_text, "min_size": 5, "max_size": 5, "min_depth": 1}),
            temp.path(),
        )
        .unwrap();
        assert_eq!(tiny.as_array().unwrap().len(), 0);
        let sized = run(
            "fs.find",
            json!({"path": root_text, "min_size": 10, "min_depth": 1}),
            temp.path(),
        )
        .unwrap();
        assert_eq!(find_names(&sized), vec![join_display(root_text, "old.log")]);
    }

    #[test]
    fn find_honours_limit_and_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..20 {
            fs::write(temp.path().join(format!("file-{index:02}.txt")), b"x").unwrap();
        }
        let root = temp.path().to_str().unwrap();
        // An explicit limit truncates the walk successfully.
        let limited = run(
            "fs.find",
            json!({"path": root, "min_depth": 1, "limit": 5}),
            temp.path(),
        )
        .unwrap();
        assert_eq!(limited.as_array().unwrap().len(), 5);
        // A cancelled budget surfaces as cancelled, not as results.
        let calls = std::cell::Cell::new(0);
        let cancel = || {
            calls.set(calls.get() + 1);
            if calls.get() > 2 {
                Err(ExecutionBudgetExceeded::Cancelled)
            } else {
                Ok(())
            }
        };
        let context = ExecutionContext::new(&cancel);
        let args = json!({"path": root, "min_depth": 1});
        let error = find(args.as_object().unwrap(), temp.path(), &context).unwrap_err();
        assert_eq!(error.code, "cancelled");
    }

    #[test]
    fn find_rejects_bad_queries_before_touching_disk() {
        let context = ExecutionContext::unrestricted();
        let cwd = Path::new("/");
        let bad = [
            json!({"path": ".", "names": []}),
            json!({"path": ".", "names": "x*"}),
            json!({"path": ".", "names": ["a/b"]}),
            json!({"path": ".", "names": ["**"]}),
            json!({"path": ".", "kinds": ["tty"]}),
            json!({"path": ".", "kinds": "file"}),
            json!({"path": ".", "min_depth": 3, "max_depth": 2}),
            json!({"path": ".", "max_depth": 257}),
            json!({"path": ".", "min_size": 9, "max_size": 2}),
            json!({"path": ".", "limit": 0}),
            json!({"path": ".", "newer_than": "soon"}),
            json!({"path": ""}),
        ];
        for args in bad {
            let error = find(args.as_object().unwrap(), cwd, &context).unwrap_err();
            assert_eq!(
                error.kind,
                crate::applets::AppletErrorKind::Permanent,
                "{args}"
            );
        }
    }

    fn find_names(value: &Value) -> Vec<String> {
        let mut names: Vec<String> = value
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    use super::set_test_mtime as set_mtime;
}

/// Set an entry's mtime to an epoch second so ordering tests are exact on
/// every platform without sleeping for clock granularity.
#[cfg(test)]
pub(crate) fn set_test_mtime(path: &Path, epoch_s: i64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let raw = CString::new(path.as_os_str().as_bytes()).unwrap();
        let stamp = libc::timespec {
            tv_sec: epoch_s as libc::time_t,
            tv_nsec: 0,
        };
        let times = [stamp, stamp];
        // SAFETY: `raw` is a NUL-terminated path alive for the call;
        // `utimensat` only reads it and the time array.
        let status = unsafe { libc::utimensat(libc::AT_FDCWD, raw.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(status, 0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
            OPEN_EXISTING,
        };
        // 1_700_000_000 epoch seconds as 100ns ticks since 1601-01-01.
        let ticks = (epoch_s as u64)
            .wrapping_mul(10_000_000)
            .wrapping_add(11_644_473_600 * 10_000_000);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated for the call; the handle is
        // closed before returning.
        unsafe {
            let handle = CreateFileW(
                wide.as_ptr(),
                FILE_WRITE_ATTRIBUTES,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            );
            assert_ne!(handle, INVALID_HANDLE_VALUE);
            let stamp = FILETIME {
                dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
                dwHighDateTime: (ticks >> 32) as u32,
            };
            let status = windows_sys::Win32::Storage::FileSystem::SetFileTime(
                handle,
                std::ptr::null(),
                std::ptr::null(),
                &stamp,
            );
            CloseHandle(handle);
            assert_ne!(status, 0);
        }
    }
}
