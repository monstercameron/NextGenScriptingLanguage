//! Cache state is data, never a symlink, device or stream endpoint.
use super::*;
use std::io::Seek;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::time::{Duration, SystemTime};

const MAX_CACHE_ENTRIES: usize = 128;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHE_DEPENDENCIES: usize = 128;
const MAX_DEPENDENCY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DEPENDENCY_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

fn directory(path: &Path, create: bool) -> Result<Option<PathBuf>> {
    // A trailing slash/dot must not cause symlink_metadata to follow the leaf.
    let path: PathBuf = path.components().collect();
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder.create(&path)?;
            fs::symlink_metadata(&path)?
        }
        Err(error) => return Err(error).context("checking cache directory"),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("cache path must be a real directory");
    }
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(&metadata) {
        bail!("cache directory cannot be a reparse point");
    }
    check_permissions(&metadata, "cache directory")?;
    Ok(Some(path))
}

fn check_permissions(metadata: &fs::Metadata, label: &str) -> Result<()> {
    #[cfg(unix)]
    {
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            bail!("{label} must be owned by the current user and private");
        }
    }
    #[cfg(windows)]
    let _ = (metadata, label);
    Ok(())
}

fn regular(metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("cached result must be a regular file");
    }
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(metadata) {
        bail!("cached result cannot be a reparse point");
    }
    check_permissions(metadata, "cached result")
}

// Keys come only from the runtime's SHA-256 identity, never a user path.
fn entry(directory: &Path, key: &str) -> PathBuf {
    directory.join(format!("{key}.bin"))
}

fn checksum_entry(directory: &Path, key: &str) -> PathBuf {
    directory.join(format!("{key}.sha256"))
}

fn read_checksum(directory: &Path, key: &str) -> Result<Option<String>> {
    let path = checksum_entry(directory, key);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => regular(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("checking cached result checksum"),
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening cached result checksum"),
    };
    regular(&file.metadata()?)?;
    let mut bytes = Vec::with_capacity(65);
    (&mut file).take(65).read_to_end(&mut bytes)?;
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Ok(None);
    }
    let text = std::str::from_utf8(&bytes).context("cache checksum is not ASCII")?;
    Ok(Some(text.to_owned()))
}

fn remove_checksum(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            regular(&metadata).context("validating cache checksum before eviction")?;
            fs::remove_file(path).context("removing cache checksum")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("checking cache checksum before eviction"),
    }
}

fn digest_hex(digest: &[u8]) -> String {
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

pub(super) fn open(path: &Path, key: &str) -> Result<Option<File>> {
    let Some(directory) = directory(path, false)? else {
        return Ok(None);
    };
    let _lock = maintenance_lock(&directory, true)?;
    let path = entry(&directory, key);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => regular(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("checking cached result"),
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening cached result"),
    };
    // Recheck the opened object, not just the pathname inspected before open.
    regular(&file.metadata()?)?;
    let Some(expected) = read_checksum(&directory, key)? else {
        return Ok(None);
    };
    let mut file = file;
    let metadata_before = file.metadata()?;
    let mut actual = Sha256::new();
    let mut buffer = [0u8; 32768];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        actual.update(&buffer[..read]);
    }
    let metadata_after = file.metadata()?;
    if !same_file_metadata(&metadata_before, &metadata_after)
        || digest_hex(&actual.finalize()) != expected
    {
        return Ok(None);
    }
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(Some(file))
}

pub(super) fn write(path: &Path, key: &str, bytes: &[u8]) -> Result<bool> {
    let directory = directory(path, true)?.expect("created cache directory");
    let _lock = maintenance_lock(&directory, false)?;
    let path = entry(&directory, key);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => regular(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("checking cache publication path"),
    }
    let checksum = checksum_entry(&directory, key);
    match fs::symlink_metadata(&checksum) {
        Ok(metadata) => regular(&metadata).context("checking cache checksum path")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("checking cache checksum path"),
    }
    // Same private temporary file, sync and atomic replacement as before.
    atomic_write(&path, bytes)?;
    let digest = digest_hex(&Sha256::digest(bytes));
    atomic_write(&checksum, digest.as_bytes())?;
    evict(&directory, &path)
}

/// Hash the manifest's explicit relative file dependencies. `Ok(None)` means
/// the dependency set could not be read safely and callers should run the
/// applet without consulting or updating its cache. Missing files are a valid,
/// fingerprinted state so creating one changes the key on the next run.
pub(super) fn dependency_fingerprint(manifest: &Manifest, cwd: &Path) -> Result<Option<[u8; 32]>> {
    let dependencies = manifest.cache_dependencies();
    if dependencies.is_empty() {
        return Ok(Some([0; 32]));
    }
    if dependencies.len() > MAX_CACHE_DEPENDENCIES {
        return Ok(None);
    }
    let Ok(root) = fs::canonicalize(cwd) else {
        return Ok(None);
    };
    if !root.is_dir() {
        return Ok(None);
    }
    fingerprint_dependencies(dependencies, &root)
}

fn fingerprint_dependencies(dependencies: &[String], root: &Path) -> Result<Option<[u8; 32]>> {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"flow-cache-dependencies-v1");
    hash_field(&mut hash, root.as_os_str().as_encoded_bytes());
    let mut total = 0u64;
    for dependency in dependencies {
        let relative = Path::new(dependency);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return Ok(None);
        }
        hash_field(&mut hash, dependency.as_bytes());
        let mut path = root.to_path_buf();
        let mut missing = false;
        let components: Vec<_> = relative.components().collect();
        if components.is_empty() {
            return Ok(None);
        }
        for (index, component) in components.iter().enumerate() {
            let std::path::Component::Normal(name) = component else {
                // A current-directory component is harmless but not useful in
                // the declared contract. Fail closed to keep identity simple.
                return Ok(None);
            };
            path.push(name);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing = true;
                    break;
                }
                Err(_) => return Ok(None),
            };
            #[cfg(windows)]
            if crate::platform::windows::is_reparse(&metadata) {
                return Ok(None);
            }
            if metadata.file_type().is_symlink() {
                return Ok(None);
            }
            if index + 1 < components.len() && !metadata.is_dir() {
                return Ok(None);
            }
            if index + 1 == components.len() && !metadata.is_file() {
                return Ok(None);
            }
        }
        if missing {
            hash_field(&mut hash, path.as_os_str().as_encoded_bytes());
            hash_field(&mut hash, b"missing");
            continue;
        }
        let canonical = match fs::canonicalize(&path) {
            Ok(canonical) if canonical.starts_with(root) => canonical,
            _ => return Ok(None),
        };
        hash_field(&mut hash, canonical.as_os_str().as_encoded_bytes());
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let Ok(mut file) = options.open(&canonical) else {
            return Ok(None);
        };
        let Ok(before) = file.metadata() else {
            return Ok(None);
        };
        if !before.is_file() || before.len() > MAX_DEPENDENCY_BYTES {
            return Ok(None);
        }
        #[cfg(windows)]
        if crate::platform::windows::is_reparse(&before) {
            return Ok(None);
        }
        total = match total.checked_add(before.len()) {
            Some(total) if total <= MAX_DEPENDENCY_TOTAL_BYTES => total,
            _ => return Ok(None),
        };
        let mut content_hash = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0u8; 32768];
        loop {
            let read = match file.read(&mut buffer) {
                Ok(read) => read,
                Err(_) => return Ok(None),
            };
            if read == 0 {
                break;
            }
            count = match count.checked_add(read as u64) {
                Some(count) if count <= MAX_DEPENDENCY_BYTES => count,
                _ => return Ok(None),
            };
            content_hash.update(&buffer[..read]);
        }
        let Ok(after) = file.metadata() else {
            return Ok(None);
        };
        if count != before.len()
            || !same_file_metadata(&before, &after)
            || fs::canonicalize(&path).ok().as_deref() != Some(canonical.as_path())
        {
            return Ok(None);
        }
        hash_field(&mut hash, b"file");
        hash_field(&mut hash, &count.to_le_bytes());
        hash_field(&mut hash, &content_hash.finalize());
    }
    let digest = hash.finalize();
    let mut output = [0u8; 32];
    output.copy_from_slice(&digest);
    Ok(Some(output))
}

fn same_file_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        left.is_file() == right.is_file()
            && left.len() == right.len()
            && left.modified().ok() == right.modified().ok()
    }
}

struct CacheLock(File);
impl Drop for CacheLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
        #[cfg(windows)]
        let _ = &self.0;
    }
}

fn maintenance_lock(directory: &Path, shared: bool) -> Result<CacheLock> {
    let path = directory.join(".maintenance.lock");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(&path)
        .context("opening cache maintenance lock")?;
    regular(&file.metadata()?)?;
    check_permissions(&file.metadata()?, "cache maintenance lock")?;
    #[cfg(unix)]
    loop {
        let lock_kind = if shared { libc::LOCK_SH } else { libc::LOCK_EX };
        if unsafe { libc::flock(file.as_raw_fd(), lock_kind | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(error).context("locking cache maintenance file");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    #[cfg(windows)]
    {
        let _ = shared; // The Windows lock is exclusive for both read and write phases.
        crate::platform::windows::lock(&file).context("locking cache maintenance file")?;
    }
    Ok(CacheLock(file))
}

fn evict(directory: &Path, current: &Path) -> Result<bool> {
    evict_with_limits(directory, current, MAX_CACHE_ENTRIES, MAX_CACHE_BYTES)
}

fn evict_with_limits(
    directory: &Path,
    current: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<bool> {
    struct Candidate {
        path: PathBuf,
        key: String,
        bytes: u64,
        checksum: PathBuf,
        modified: SystemTime,
    }
    let mut candidates = Vec::new();
    let mut total = 0u64;
    for entry in fs::read_dir(directory).context("scanning cache for eviction")? {
        let entry = entry.context("reading cache directory entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(key) = name.strip_suffix(".bin") else {
            continue;
        };
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).context("checking cache eviction entry")?;
        regular(&metadata).context("validating cache eviction entry")?;
        let checksum = checksum_entry(directory, key);
        let checksum_bytes = match fs::symlink_metadata(&checksum) {
            Ok(checksum_metadata) => {
                regular(&checksum_metadata).context("validating cache checksum entry")?;
                checksum_metadata.len()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error).context("checking cache checksum entry"),
        };
        let bytes = metadata.len().saturating_add(checksum_bytes);
        total = total.saturating_add(bytes);
        candidates.push(Candidate {
            path,
            key: key.to_owned(),
            bytes,
            checksum,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    candidates.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.key.cmp(&right.key))
    });
    let mut count = candidates.len();
    for candidate in candidates {
        if count <= max_entries && total <= max_bytes {
            break;
        }
        // Keep the just-written value unless it alone exceeds the entire
        // budget. Oversized results should be returned to the caller but not
        // retained as cache storage.
        let is_current = candidate.path == current;
        if is_current && candidate.bytes <= max_bytes {
            continue;
        }
        match fs::remove_file(&candidate.path) {
            Ok(()) => {
                remove_checksum(&candidate.checksum)?;
                count = count.saturating_sub(1);
                total = total.saturating_sub(candidate.bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remove_checksum(&candidate.checksum)?;
                count = count.saturating_sub(1);
                total = total.saturating_sub(candidate.bytes);
            }
            Err(error) => return Err(error).context("evicting cache entry"),
        }
    }
    Ok(fs::symlink_metadata(current).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn ordinary_cache_bytes_and_capture_limits_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("new/nested/cache");
        assert!(open(&directory, KEY).unwrap().is_none());
        assert!(!directory.exists());
        let bytes = b"a\0\xff\n";
        write(&directory, KEY, bytes).unwrap();
        assert_eq!(
            capture(open(&directory, KEY).unwrap().unwrap(), 4).unwrap(),
            bytes
        );
        assert!(capture(open(&directory, KEY).unwrap().unwrap(), 3).is_err());
        write(&directory, KEY, b"replaced").unwrap();
        assert_eq!(fs::read(entry(&directory, KEY)).unwrap(), b"replaced");
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bin"))
                .count(),
            1
        );
        #[cfg(unix)]
        {
            assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
            assert_eq!(
                fs::metadata(entry(&directory, KEY)).unwrap().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn tampered_bytes_or_checksum_are_safe_cache_misses() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("cache");
        write(&directory, KEY, b"trusted output").unwrap();

        fs::write(entry(&directory, KEY), b"tampered output").unwrap();
        assert!(open(&directory, KEY).unwrap().is_none());

        write(&directory, KEY, b"trusted output").unwrap();
        fs::write(checksum_entry(&directory, KEY), b"0".repeat(64)).unwrap();
        assert!(open(&directory, KEY).unwrap().is_none());
    }

    #[test]
    fn non_directory_root_and_non_regular_entry_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, b"unchanged").unwrap();
        assert!(open(&file, KEY).is_err());
        assert!(write(&file, KEY, b"replacement").is_err());
        assert_eq!(fs::read(&file).unwrap(), b"unchanged");
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        fs::create_dir(entry(&cache, KEY)).unwrap();
        assert!(open(&cache, KEY).is_err());
        assert!(write(&cache, KEY, b"replacement").is_err());
        assert!(entry(&cache, KEY).is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cache_directories_and_entries_are_rejected_without_target_changes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("cache-link");
        symlink(&target, &link).unwrap();
        let before = fs::metadata(&target).unwrap().mode();
        for suffix in ["", "/", "/."] {
            let path = PathBuf::from(format!("{}{suffix}", link.display()));
            assert!(open(&path, KEY).is_err());
            assert!(write(&path, KEY, b"replacement").is_err());
        }
        assert_eq!(fs::metadata(&target).unwrap().mode(), before);
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        let canary = temp.path().join("canary");
        fs::write(&canary, b"owned private canary").unwrap();
        fs::set_permissions(&canary, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&canary, entry(&target, KEY)).unwrap();
        assert!(open(&target, KEY).is_err());
        assert!(write(&target, KEY, b"replacement").is_err());
        assert_eq!(fs::read(&canary).unwrap(), b"owned private canary");
        assert!(fs::symlink_metadata(entry(&target, KEY))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn special_entries_and_untrusted_permissions_are_rejected() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let path = entry(&cache, KEY);
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open(&cache, KEY).is_err());
        assert!(write(&cache, KEY, b"replacement").is_err());
        fs::remove_file(&path).unwrap();
        let socket_path = temp.path().join("socket");
        let socket = UnixListener::bind(&socket_path).unwrap();
        fs::rename(&socket_path, &path).unwrap();
        assert!(open(&cache, KEY).is_err());
        assert!(write(&cache, KEY, b"replacement").is_err());
        drop(socket);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"original").unwrap();
        for mode in [0o620, 0o602, 0o644, 0o666] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(open(&cache, KEY).is_err());
            assert!(write(&cache, KEY, b"replacement").is_err());
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        for mode in [0o744, 0o770, 0o707, 0o777] {
            fs::set_permissions(&cache, fs::Permissions::from_mode(mode)).unwrap();
            assert!(open(&cache, KEY).is_err());
            assert!(write(&cache, KEY, b"replacement").is_err());
        }
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"original");
    }

    #[test]
    fn declared_dependency_fingerprints_track_bytes_and_missing_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.jsonc");
        // Production canonicalizes cwd before hashing; do the same here on
        // platforms where /var or the temp root includes a symlink prefix.
        let root = fs::canonicalize(temp.path()).unwrap();
        let dependencies = vec!["settings.jsonc".to_owned()];
        let missing = fingerprint_dependencies(&dependencies, &root)
            .unwrap()
            .unwrap();
        fs::write(&path, b"theme=light\n").unwrap();
        let present = fingerprint_dependencies(&dependencies, &root)
            .unwrap()
            .unwrap();
        assert_ne!(missing, present);
        fs::write(&path, b"theme=dark\n").unwrap();
        let changed = fingerprint_dependencies(&dependencies, &root)
            .unwrap()
            .unwrap();
        assert_ne!(present, changed);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(&path).unwrap();
            let outside = temp.path().join("outside");
            fs::write(&outside, b"outside").unwrap();
            symlink(&outside, &path).unwrap();
            assert!(fingerprint_dependencies(&dependencies, &root)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn eviction_keeps_a_bounded_newest_value_and_drops_oversized_entries() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("cache");
        fs::create_dir(&directory).unwrap();
        let keys = [
            "0000000000000000000000000000000000000000000000000000000000000000",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222222222222222222222222222",
        ];
        for key in keys {
            let path = entry(&directory, key);
            fs::write(&path, b"abc").unwrap();
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let current = entry(&directory, keys[2]);
        assert!(evict_with_limits(&directory, &current, 3, 5).unwrap());
        assert!(!entry(&directory, keys[0]).exists());
        assert!(!entry(&directory, keys[1]).exists());
        assert!(current.exists());

        fs::write(&current, b"too large").unwrap();
        assert!(!evict_with_limits(&directory, &current, 3, 4).unwrap());
        assert!(!current.exists());
    }
}
