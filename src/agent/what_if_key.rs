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
//! these is refused, never used. Windows files take the owner-only ACL of the user
//! profile directory; this module does not inspect or set the ACL.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
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
    let mut file =
        File::open(path).with_context(|| format!("reading the what-if key {}", path.display()))?;
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

/// Create a new key at `path` with owner-only permissions. If another process creates
/// the file first, its key is read instead, so the two never disagree.
fn create(path: &Path) -> Result<[u8; KEY_BYTES]> {
    let parent = path
        .parent()
        .context("the what-if key path has no directory")?;
    create_private_dir(parent)?;
    let mut key = [0u8; KEY_BYTES];
    getrandom::fill(&mut key)
        .map_err(|error| anyhow::anyhow!("generating the what-if key: {error}"))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&key)
                .and_then(|()| file.sync_all())
                .with_context(|| format!("writing the what-if key {}", path.display()))?;
            Ok(key)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => read_existing(path)?
            .context("the what-if key was created by another process but could not be read"),
        Err(error) => {
            Err(error).with_context(|| format!("creating the what-if key {}", path.display()))
        }
    }
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
