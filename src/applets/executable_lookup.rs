use super::{AppletError, ExecutionContext, invalid};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn which(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let names: Vec<&str> = match args.get("name") {
        Some(Value::String(name)) => vec![name],
        Some(_) => return Err(invalid("args.name must be a string")),
        None => args
            .get("names")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("args.name or args.names must be provided"))?
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_str()
                    .ok_or_else(|| invalid(format!("args.names[{index}] must be a string")))
            })
            .collect::<Result<_, _>>()?,
    };
    if names.is_empty()
        || names
            .iter()
            .any(|name| name.is_empty() || name.contains('\0'))
    {
        return Err(invalid("command names must be nonempty and contain no NUL"));
    }
    let search_path = match args.get("path") {
        None => std::env::var_os("PATH").unwrap_or_default(),
        Some(Value::String(path)) => path.into(),
        Some(_) => return Err(invalid("args.path must be a string")),
    };
    let mut all = Vec::new();
    for name in names {
        context.checkpoint()?;
        for found in find_all(name, &search_path, cwd) {
            all.push(Value::String(found.to_string_lossy().into_owned().into()));
        }
    }
    let path = all.first().cloned().unwrap_or(Value::Null);
    Ok(serde_json::json!({ "found": !all.is_empty(), "path": path, "all": all }))
}

fn find_all(name: &str, search_path: &std::ffi::OsStr, cwd: &Path) -> Vec<PathBuf> {
    let name_path = Path::new(name);
    #[cfg(windows)]
    let has_separator = name.contains('/') || name.contains('\\');
    #[cfg(not(windows))]
    let has_separator = name.contains('/');
    let candidates = if has_separator || name_path.is_absolute() {
        vec![if name_path.is_absolute() {
            name_path.to_path_buf()
        } else {
            cwd.join(name_path)
        }]
    } else {
        let mut candidates = Vec::new();
        for directory in std::env::split_paths(search_path) {
            let directory = if directory.is_absolute() {
                directory
            } else {
                cwd.join(directory)
            };
            for extension in extensions(name_path) {
                let mut candidate = directory.join(name);
                if !extension.is_empty() {
                    candidate.set_extension(extension);
                }
                candidates.push(candidate);
            }
        }
        candidates
    };
    let mut found = Vec::new();
    for candidate in candidates {
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if crate::platform::executable(&metadata) && !found.contains(&candidate) {
            found.push(candidate);
        }
    }
    found
}

#[cfg(windows)]
fn extensions(name: &Path) -> Vec<String> {
    if name.extension().is_some() {
        return vec![String::new()];
    }
    std::env::var_os("PATHEXT")
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .to_string_lossy()
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.trim_start_matches('.').to_owned())
        .collect()
}

#[cfg(not(windows))]
fn extensions(_: &Path) -> Vec<String> {
    vec![String::new()]
}

#[cfg(test)]
mod tests {
    use super::find_all;
    use std::fs;

    #[test]
    fn lookup_skips_directories_and_resolves_slash_names_from_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("probe.exe");
        fs::write(&executable, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::create_dir(temp.path().join("folder.exe")).unwrap();
        let path = temp.path().as_os_str();
        assert_eq!(find_all("probe.exe", path, temp.path()), vec![executable]);
        assert!(find_all("folder.exe", path, temp.path()).is_empty());
        assert_eq!(
            find_all("./probe.exe", std::ffi::OsStr::new(""), temp.path()).len(),
            1
        );
    }
}
