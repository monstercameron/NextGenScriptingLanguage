//! Small OS boundary shared by the runtime, checkpoints and control transport.
use anyhow::Result;
use std::fs::{Metadata, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Arc};

pub(crate) mod sandbox;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) fn executable_path(path: PathBuf) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        Ok(path)
    }
    #[cfg(windows)]
    {
        let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd") {
            anyhow::bail!("batch files require an explicit cmd.exe invocation; native calls never insert a shell");
        }
        if extension.is_empty() {
            Ok(path.with_extension("exe"))
        } else {
            Ok(path)
        }
    }
}
pub(crate) fn executable(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        metadata.is_file()
    }
}
pub(crate) fn private_options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
pub(crate) fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(source, target)
    }
    #[cfg(windows)]
    {
        windows::replace_file(source, target)
    }
}
pub(crate) fn signals(cancel: Arc<AtomicBool>) -> Result<Vec<signal_hook::SigId>> {
    #[cfg(unix)]
    {
        let mut ids = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
            match signal_hook::flag::register(signal, cancel.clone()) {
                Ok(id) => ids.push(id),
                Err(error) => {
                    for id in ids {
                        signal_hook::low_level::unregister(id);
                    }
                    return Err(error.into());
                }
            }
        }
        Ok(ids)
    }
    #[cfg(windows)]
    {
        windows::signals(cancel)?;
        Ok(Vec::new())
    }
}
