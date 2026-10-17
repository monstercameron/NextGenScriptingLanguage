//! Home-directory expansion for path-typed values. Pure std, so the browser
//! build has it too; there it fails because no home directory is set.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Expand a leading `~` or `~/` (or `~\` on Windows) to the user's home directory,
/// for path-typed values only. Argv strings are never passed through here, so a
/// literal `~` stays literal there. `~user` is not supported and stays literal.
pub fn expand_home_path(path: &str) -> Result<PathBuf> {
    let rest = if path == "~" {
        Some("")
    } else if let Some(rest) = path.strip_prefix("~/") {
        Some(rest)
    } else if cfg!(windows) {
        path.strip_prefix("~\\")
    } else {
        None
    };
    let Some(rest) = rest else {
        return Ok(PathBuf::from(path));
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("cannot expand ~: no home directory is set in the environment")?;
    Ok(home.join(rest))
}
