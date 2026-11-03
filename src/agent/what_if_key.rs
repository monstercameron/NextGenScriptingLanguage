//! The per-user key that keys the what-if digest of environment-sourced input values.
//!
//! A value from an environment input is a candidate secret. An unkeyed hash of a
//! low-entropy secret can be brute-forced offline, so the review digests it with
//! HMAC-SHA256 under this key instead. The key is 32 random bytes, created on first
//! use. It is never printed, and it is kept where only the user can read it.
//!
//! The key location is `$XDG_STATE_HOME/execute/what-if.key`, or
//! `~/.local/state/execute/what-if.key` when that variable is unset or relative. On
//! Windows it is `%LOCALAPPDATA%\execute\what-if.key`.
//!
//! On Unix the key file must be a regular file, owned by no one but the user (no
//! group or other permission bits), and exactly 32 bytes. A file that fails any of
//! these is refused, never used. Windows files are created with a protected DACL and
//! explicit current-user owner; existing files are checked before the key is read.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(all(not(windows), not(unix)))]
use std::fs::File;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// Length of the key in bytes.
pub const KEY_BYTES: usize = 32;

/// SHA-256 block size, in bytes. HMAC pads its key to this length.
const BLOCK_BYTES: usize = 64;

/// HMAC-SHA256 (RFC 2104) of `message` under `key`. Keys longer than the block are
/// hashed first, as the standard requires.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner_pad: Vec<u8> = block.iter().map(|byte| byte ^ 0x36).collect();
    let outer_pad: Vec<u8> = block.iter().map(|byte| byte ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// The key file's path for this user.
pub fn key_path() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .context("LOCALAPPDATA is not set, so the what-if key has no location")?;
        Ok(PathBuf::from(base).join("execute").join("what-if.key"))
    }
    #[cfg(not(windows))]
    {
        let state = match std::env::var_os("XDG_STATE_HOME") {
            Some(path) if Path::new(&path).is_absolute() => PathBuf::from(path),
            _ => {
                let home = std::env::var_os("HOME")
                    .context("HOME is not set, so the what-if key has no location")?;
                PathBuf::from(home).join(".local").join("state")
            }
        };
        Ok(state.join("execute").join("what-if.key"))
    }
}

/// The key at the user's location, created on first use.
pub fn load_or_create() -> Result<[u8; KEY_BYTES]> {
    load_or_create_at(&key_path()?)
}

/// The key at `path`, created there when it does not exist. Fails closed when the
/// key cannot be created or read safely.
pub fn load_or_create_at(path: &Path) -> Result<[u8; KEY_BYTES]> {
    match read_existing(path)? {
        Some(key) => Ok(key),
        None => create(path),
    }
}

/// Read the existing per-user key without creating a replacement. Resume paths
/// use this when a checkpoint records keyed inputs, because a new key cannot
/// authenticate the inputs already stored in its journal.
pub fn load_existing() -> Result<[u8; KEY_BYTES]> {
    load_existing_at(&key_path()?)
}

/// Read an existing key without creating one, and explain how to recover if it
/// is missing.
pub fn load_existing_at(path: &Path) -> Result<[u8; KEY_BYTES]> {
    read_existing(path)?.with_context(|| {
        format!(
            "the per-user input key is missing at {}; restore that key from backup before resuming this checkpoint",
            path.display()
        )
    })
}

/// Read an existing key, refusing any file that is not a private 32-byte regular file.
fn read_existing(path: &Path) -> Result<Option<[u8; KEY_BYTES]>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading the what-if key {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "the what-if key {} is not a regular file; remove it and run again to create a new key",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "the what-if key {} is readable by other users; set its mode to 0600, or remove it to create a new key",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    let mut file = crate::security::open_private_file(path)
        .with_context(|| format!("reading the what-if key {}", path.display()))?;
    // Open without following a final symlink and without blocking on a special
    // file, then validate the opened descriptor itself: the pathname checks
    // above can be raced by a replacement between check and open.
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC);
        match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading the what-if key {}", path.display()))
            }
        }
    };
    #[cfg(all(not(windows), not(unix)))]
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading the what-if key {}", path.display()))
        }
    };
    let opened = file
        .metadata()
        .with_context(|| format!("checking the what-if key {}", path.display()))?;
    #[cfg(unix)]
    validate_opened_key(path, &opened)?;
    #[cfg(not(unix))]
    if !opened.is_file() {
        bail!(
            "the what-if key {} is not a regular file; remove it and run again to create a new key",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(KEY_BYTES + 1);
    (&mut file)
        .take(KEY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading the what-if key {}", path.display()))?;
    if bytes.len() != KEY_BYTES {
        bail!(
            "the what-if key {} is not {KEY_BYTES} bytes; remove it to create a new key",
            path.display()
        );
    }
    let mut key = [0u8; KEY_BYTES];
    key.copy_from_slice(&bytes);
    Ok(Some(key))
}

/// Validate the opened key descriptor on Unix: a regular file owned by the
/// effective user with no group or other permission bits.
#[cfg(unix)]
fn validate_opened_key(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    check_opened_key(
        path,
        metadata.is_file(),
        metadata.uid(),
        metadata.mode(),
        crate::platform::unix_ffi::effective_uid(),
    )
}

#[cfg(unix)]
fn check_opened_key(path: &Path, is_file: bool, uid: u32, mode: u32, euid: u32) -> Result<()> {
    if !is_file {
        bail!(
            "the what-if key {} is not a regular file; remove it and run again to create a new key",
            path.display()
        );
    }
    if uid != euid {
        bail!(
            "the what-if key {} is not owned by the current user; remove it and run again to create a new key",
            path.display()
        );
    }
    if mode & 0o077 != 0 {
        bail!(
            "the what-if key {} is readable by other users; set its mode to 0600, or remove it to create a new key",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod missing_key_tests {
    use super::load_existing_at;

    #[test]
    fn reading_a_missing_checkpoint_key_never_creates_a_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("execute").join("what-if.key");
        let error = load_existing_at(&path).expect_err("a missing key must be refused");
        let message = format!("{error:#}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(
            message.contains("restore that key from backup"),
            "{message}"
        );
        assert!(!path.exists(), "reading a missing key must not create one");
    }
}

/// Create a new key at `path` with owner-only permissions. If another process creates
/// the file first, its key is read instead, so the two never disagree.
///
/// The key is written to a private temporary name and published with a hard link,
/// which fails when `path` already exists. A concurrent reader therefore sees either
/// no key or the whole key, never an empty file that would be refused as corrupt.
fn create(path: &Path) -> Result<[u8; KEY_BYTES]> {
    let parent = path
        .parent()
        .context("the what-if key path has no directory")?;
    create_private_dir(parent)?;
    let mut key = [0u8; KEY_BYTES];
    getrandom::fill(&mut key)
        .map_err(|error| anyhow::anyhow!("generating the what-if key: {error}"))?;
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix)
        .map_err(|error| anyhow::anyhow!("generating a temporary key name: {error}"))?;
    let temporary = parent.join(format!(
        ".what-if.key.{:016x}.tmp",
        u64::from_le_bytes(suffix)
    ));
    let published = write_private(&temporary, &key)
        .and_then(|()| fs::hard_link(&temporary, path).map_err(anyhow::Error::from));
    let _ = fs::remove_file(&temporary);
    match published {
        Ok(()) => Ok(key),
        Err(error) if is_already_exists(&error) => read_existing(path)?
            .context("the what-if key was created by another process but could not be read"),
        Err(error) => {
            Err(error).with_context(|| format!("creating the what-if key {}", path.display()))
        }
    }
}

/// Write `key` to a new owner-only file at `path` and flush it to disk.
fn write_private(path: &Path, key: &[u8]) -> Result<()> {
    let mut file = open_new_private(path)?;
    file.write_all(key)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing the what-if key {}", path.display()))
}

#[cfg(windows)]
fn open_new_private(path: &Path) -> Result<fs::File> {
    crate::platform::windows::create_private_file(path)
}

#[cfg(not(windows))]
fn open_new_private(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

fn is_already_exists(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == ErrorKind::AlreadyExists)
    })
}

#[cfg(unix)]
fn create_private_dir(directory: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .with_context(|| format!("creating {}", directory.display()))
}

#[cfg(not(unix))]
fn create_private_dir(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory).with_context(|| format!("creating {}", directory.display()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn private_path() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("execute").join("what-if.key");
        (directory, path)
    }

    #[test]
    fn a_new_key_is_32_bytes_owner_only_and_stable() {
        let (_directory, path) = private_path();
        let first = load_or_create_at(&path).unwrap();
        let second = load_or_create_at(&path).unwrap();
        assert_eq!(first, second, "the key must not change once created");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the key file must be owner-only");
        assert_eq!(fs::read(&path).unwrap().len(), KEY_BYTES);
    }

    #[test]
    fn concurrent_first_use_sees_one_whole_key() {
        let (_directory, path) = private_path();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || load_or_create_at(&path))
            })
            .collect();
        let keys: Vec<_> = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap()
                    .expect("every racer gets a whole key")
            })
            .collect();
        assert!(keys.iter().all(|key| *key == keys[0]), "racers disagreed");
    }

    #[test]
    fn a_group_or_world_readable_key_is_refused_and_never_used() {
        let (_directory, path) = private_path();
        load_or_create_at(&path).unwrap();
        for mode in [0o640, 0o604, 0o644] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let error = load_or_create_at(&path).expect_err("an open key must be refused");
            assert!(
                format!("{error:#}").contains("readable by other users"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused() {
        let (_directory, path) = private_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [7u8; 31]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = load_or_create_at(&path).expect_err("a short key must be refused");
        assert!(
            format!("{error:#}").contains("is not 32 bytes"),
            "{error:#}"
        );
    }

    #[test]
    fn an_opened_key_owned_by_another_user_is_refused() {
        let (_directory, path) = private_path();
        // A private regular file whose owner differs from the effective uid.
        let error = check_opened_key(&path, true, 65534, 0o600, 1000)
            .expect_err("a foreign-owned key must be refused");
        assert!(
            format!("{error:#}").contains("not owned by the current user"),
            "{error:#}"
        );
        // The same file owned by the effective user passes, and a substituted
        // non-regular object or group/other bits on the opened fd do not.
        check_opened_key(&path, true, 1000, 0o600, 1000).unwrap();
        assert!(check_opened_key(&path, false, 1000, 0o600, 1000).is_err());
        assert!(check_opened_key(&path, true, 1000, 0o640, 1000).is_err());
    }

    #[test]
    fn a_foreign_owned_key_is_refused_when_chown_is_available() {
        use std::os::unix::ffi::OsStrExt;
        let (_directory, path) = private_path();
        load_or_create_at(&path).unwrap();
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the C path is NUL-terminated and valid for the chown call.
        let changed = unsafe { libc::chown(c_path.as_ptr(), 65534, u32::MAX) } == 0;
        if !changed {
            // Changing ownership needs privileges; keep this active on
            // privileged builders while ordinary runs stay portable.
            return;
        }
        let error = load_or_create_at(&path).expect_err("a foreign-owned key must be refused");
        assert_eq!(
            // SAFETY: as above; restoring the owner lets TempDir clean up.
            unsafe { libc::chown(c_path.as_ptr(), libc::geteuid(), u32::MAX) },
            0
        );
        assert!(
            format!("{error:#}").contains("not owned by the current user"),
            "{error:#}"
        );
    }

    #[test]
    fn a_fifo_at_the_key_path_is_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let (_directory, path) = private_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the C path is NUL-terminated and valid for the mkfifo call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let error = load_or_create_at(&path).expect_err("a FIFO must be refused, not read");
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
    }

    #[test]
    fn a_symlink_is_never_followed_as_the_key() {
        let (directory, path) = private_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = directory.path().join("elsewhere");
        fs::write(&target, [9u8; KEY_BYTES]).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let error = load_or_create_at(&path).expect_err("a symlink must be refused");
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
    }
}

#[cfg(test)]
mod hmac_vectors {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// RFC 4231 test case 1: a 20-byte key of 0x0b.
    #[test]
    fn hmac_matches_rfc_4231_case_1() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// RFC 4231 test case 2: a short textual key.
    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// RFC 4231 test case 6: a key longer than the block, which is hashed first.
    #[test]
    fn hmac_hashes_a_key_longer_than_the_block() {
        let key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac_sha256(
                &key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
