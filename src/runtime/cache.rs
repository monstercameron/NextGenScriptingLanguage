//! Cache state is data, never a symlink, device or stream endpoint.
use super::*;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

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
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            bail!("{label} must be owned by the current user and not writable by other users");
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

pub(super) fn open(path: &Path, key: &str) -> Result<Option<File>> {
    let Some(directory) = directory(path, false)? else {
        return Ok(None);
    };
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
    Ok(Some(file))
}

pub(super) fn write(path: &Path, key: &str, bytes: &[u8]) -> Result<()> {
    let directory = directory(path, true)?.expect("created cache directory");
    let path = entry(&directory, key);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => regular(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("checking cache publication path"),
    }
    // Same private temporary file, sync and atomic replacement as before.
    atomic_write(&path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
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
        for mode in [0o620, 0o602, 0o666] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(open(&cache, KEY).is_err());
            assert!(write(&cache, KEY, b"replacement").is_err());
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        for mode in [0o770, 0o707, 0o777] {
            fs::set_permissions(&cache, fs::Permissions::from_mode(mode)).unwrap();
            assert!(open(&cache, KEY).is_err());
            assert!(write(&cache, KEY, b"replacement").is_err());
        }
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"original");
    }
}
