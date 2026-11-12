//! Restart-required detection from kernel, binaries and marker files (#343).
//!
//! [`assess`] answers "does this machine need a reboot or a service restart after an
//! update?" from three sources, all read beneath a root directory (`/` in production, a
//! temporary tree in tests):
//!
//! 1. Marker files written by distro tooling: `var/run/reboot-required` and, when present,
//!    `var/run/reboot-required.pkgs` (Debian and Ubuntu, written by update-notifier-common).
//! 2. The kernel: `proc/sys/kernel/osrelease` compared with the module trees under
//!    `usr/lib/modules` and `lib/modules`.
//! 3. Processes that run a binary or library replaced on disk: `proc/<pid>/exe` and
//!    `proc/<pid>/maps` entries whose path ends in ` (deleted)`.
//!
//! A missing file or directory is an absent reason, never an error.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MARKER: &str = "var/run/reboot-required";
const MARKER_PKGS: &str = "var/run/reboot-required.pkgs";
/// Bytes read from the `.pkgs` file at most.
const PKGS_MAX_BYTES: u64 = 64 * 1024;
/// Package names reported at most.
const PKGS_MAX_NAMES: usize = 256;
/// Bytes read from one `/proc/<pid>/maps` file at most.
const MAPS_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Replaced paths reported per process at most.
const DELETED_PER_PROCESS: usize = 8;
/// Replaced-binary reasons reported in total at most.
const DELETED_TOTAL: usize = 256;
const DELETED_SUFFIX: &str = " (deleted)";

/// One reason that a restart or reboot may be needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    /// A distro marker file asks for a reboot.
    RebootFlag {
        path: String,
        packages: Vec<String>,
        packages_truncated: bool,
    },
    /// The running kernel release is compared with the installed module trees. Reboot is
    /// required only when the running release has no module tree any more; a different
    /// release that is merely also installed is informational.
    Kernel {
        running: String,
        installed: Vec<String>,
        reboot_required: bool,
    },
    /// A process runs a binary or library that was replaced on disk.
    ReplacedBinary {
        pid: u32,
        command: Option<String>,
        path: String,
    },
}

impl Reason {
    /// True when this reason calls for a reboot.
    pub fn needs_reboot(&self) -> bool {
        match self {
            Reason::RebootFlag { .. } => true,
            Reason::Kernel {
                reboot_required, ..
            } => *reboot_required,
            Reason::ReplacedBinary { .. } => false,
        }
    }

    /// True when this reason calls for restarting the affected process.
    pub fn needs_restart(&self) -> bool {
        matches!(self, Reason::ReplacedBinary { .. })
    }
}

/// The single value a program branches on: the strongest action any reason calls for.
/// `Reboot` outranks `Restart`, and `Restart` outranks `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    #[default]
    None,
    Restart,
    Reboot,
}

/// The typed answer of [`assess`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Assessment {
    pub reboot_required: bool,
    pub restart_required: bool,
    /// `reboot` when `reboot_required`, else `restart` when `restart_required`, else `none`.
    pub outcome: Outcome,
    pub reasons: Vec<Reason>,
    /// True when the process scan stopped at a bound, so more replaced binaries may exist.
    pub truncated: bool,
}

/// Reads the target of a `/proc` link. Production uses the filesystem; tests substitute a
/// table because Windows test runners cannot create Unix symlinks reliably.
trait LinkSource {
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;
}

struct FsLinks;

impl LinkSource for FsLinks {
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        fs::read_link(path)
    }
}

/// Assesses the restart and reboot state of the system rooted at `root`.
pub fn assess(root: &Path) -> Assessment {
    assess_with(root, &FsLinks)
}

fn assess_with(root: &Path, links: &dyn LinkSource) -> Assessment {
    let mut reasons = Vec::new();
    reasons.extend(marker_reason(root));
    reasons.extend(kernel_reason(root));
    let (binaries, truncated) = replaced_binaries(root, links);
    reasons.extend(binaries);
    let reboot_required = reasons.iter().any(Reason::needs_reboot);
    let restart_required = reasons.iter().any(Reason::needs_restart);
    let outcome = if reboot_required {
        Outcome::Reboot
    } else if restart_required {
        Outcome::Restart
    } else {
        Outcome::None
    };
    Assessment {
        reboot_required,
        restart_required,
        outcome,
        reasons,
        truncated,
    }
}

fn marker_reason(root: &Path) -> Option<Reason> {
    if !root.join(MARKER).is_file() {
        return None;
    }
    let (packages, packages_truncated) = read_package_names(&root.join(MARKER_PKGS));
    Some(Reason::RebootFlag {
        path: MARKER.to_string(),
        packages,
        packages_truncated,
    })
}

/// Reads one package name per line, skipping blank lines, within the byte and name bounds.
/// An unreadable file yields no names.
fn read_package_names(path: &Path) -> (Vec<String>, bool) {
    let mut bytes = Vec::new();
    let read = fs::File::open(path).and_then(|f| f.take(PKGS_MAX_BYTES).read_to_end(&mut bytes));
    if read.is_err() {
        return (Vec::new(), false);
    }
    let mut truncated = bytes.len() as u64 >= PKGS_MAX_BYTES;
    let mut names = Vec::new();
    for line in String::from_utf8_lossy(&bytes).lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        if names.len() == PKGS_MAX_NAMES {
            truncated = true;
            break;
        }
        names.push(name.to_string());
    }
    (names, truncated)
}

/// Compares the running kernel release with the installed module trees.
fn kernel_reason(root: &Path) -> Option<Reason> {
    let running = fs::read_to_string(root.join("proc/sys/kernel/osrelease")).ok()?;
    let running = running.trim().to_string();
    if running.is_empty()
        || running == "."
        || running == ".."
        || running.chars().any(|c| c == '/' || c == '\\')
    {
        return None;
    }
    let mut installed = BTreeSet::new();
    for tree in ["usr/lib/modules", "lib/modules"] {
        let Ok(entries) = fs::read_dir(root.join(tree)) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if entry.path().is_dir() {
                    installed.insert(name.to_string());
                }
            }
        }
    }
    if installed.is_empty() {
        return None;
    }
    let running_present = installed.contains(&running);
    if running_present && installed.len() == 1 {
        return None;
    }
    Some(Reason::Kernel {
        running,
        installed: installed.into_iter().collect(),
        reboot_required: !running_present,
    })
}

/// Scans every numeric `proc/<pid>` for replaced executables and libraries. Entries that
/// cannot be read are skipped silently.
fn replaced_binaries(root: &Path, links: &dyn LinkSource) -> (Vec<Reason>, bool) {
    let mut pids: Vec<u32> = match fs::read_dir(root.join("proc")) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(pid_of))
            .collect(),
        Err(_) => return (Vec::new(), false),
    };
    pids.sort_unstable();

    let mut reasons = Vec::new();
    let mut truncated = false;
    'processes: for pid in pids {
        let dir = root.join("proc").join(pid.to_string());
        let mut paths = BTreeSet::new();
        if let Ok(target) = links.read_link(&dir.join("exe")) {
            if let Some(path) = deleted_path(&target.to_string_lossy()) {
                paths.insert(path.to_string());
            }
        }
        if let Ok(maps) = read_maps(&dir.join("maps")) {
            for line in maps.lines() {
                if let Some(path) = maps_pathname(line).and_then(deleted_path) {
                    paths.insert(path.to_string());
                }
            }
        }
        if paths.is_empty() {
            continue;
        }
        let command = fs::read_to_string(dir.join("comm"))
            .ok()
            .map(|c| c.trim_end().to_string());
        for (count, path) in paths.into_iter().enumerate() {
            if count == DELETED_PER_PROCESS {
                truncated = true;
                break;
            }
            if reasons.len() == DELETED_TOTAL {
                truncated = true;
                break 'processes;
            }
            reasons.push(Reason::ReplacedBinary {
                pid,
                command: command.clone(),
                path,
            });
        }
    }
    (reasons, truncated)
}

fn pid_of(name: &str) -> Option<u32> {
    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
        name.parse().ok()
    } else {
        None
    }
}

fn read_maps(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAPS_MAX_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The pathname column of a `/proc/<pid>/maps` line. Per proc(5) the first five fields are
/// address, permissions, offset, device and inode; the remainder of the line is the
/// pathname, which may contain spaces.
fn maps_pathname(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    for _ in 0..5 {
        let end = rest.find(char::is_whitespace)?;
        rest = rest[end..].trim_start();
    }
    let path = rest.trim_end();
    (!path.is_empty()).then_some(path)
}

/// The replaced path named by a link target or maps pathname, or `None` when the entry is
/// not one. proc(5) describes the kernel appending " (deleted)" to the name of an unlinked
/// file. The exclusion of anonymous memory is this module's rule, not a kernel rule: memfd
/// regions appear as "/memfd:<name>", SysV shared memory as "/SYSV<key>" and POSIX shared
/// memory under "/dev/shm/". None of these is a file a package update replaces.
fn deleted_path(entry: &str) -> Option<&str> {
    let path = entry.strip_suffix(DELETED_SUFFIX)?;
    let anonymous =
        path.starts_with("/memfd:") || path.starts_with("/SYSV") || path.starts_with("/dev/shm/");
    (path.starts_with('/') && !anonymous).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A link table: paths missing from it fail to read, as an unreadable process would.
    struct Table(HashMap<PathBuf, PathBuf>);

    impl LinkSource for Table {
        fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// Adds a process directory with comm, optional maps and an optional exe link target.
    fn process(
        root: &Path,
        links: &mut HashMap<PathBuf, PathBuf>,
        pid: u32,
        comm: &str,
        exe: Option<&str>,
        maps: Option<&str>,
    ) {
        let dir = root.join("proc").join(pid.to_string());
        write(root, &format!("proc/{pid}/comm"), comm);
        if let Some(maps) = maps {
            write(root, &format!("proc/{pid}/maps"), maps);
        }
        if let Some(exe) = exe {
            links.insert(dir.join("exe"), PathBuf::from(exe));
        }
    }

    fn kernel(root: &Path, running: &str, trees: &[&str]) {
        write(root, "proc/sys/kernel/osrelease", &format!("{running}\n"));
        for tree in trees {
            fs::create_dir_all(root.join("usr/lib/modules").join(tree)).unwrap();
        }
    }

    fn run(root: &Path, links: &HashMap<PathBuf, PathBuf>) -> Assessment {
        assess_with(root, &Table(links.clone()))
    }

    #[test]
    fn clean_root_has_no_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let a = assess(dir.path());
        assert_eq!(a, Assessment::default());
    }

    #[test]
    fn missing_root_is_absent_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let a = assess(&dir.path().join("does-not-exist"));
        assert_eq!(a, Assessment::default());
    }

    #[test]
    fn marker_alone_with_packages() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), MARKER, "");
        write(
            dir.path(),
            MARKER_PKGS,
            "linux-image-6.8.0-40-generic\n\n  linux-base  \n",
        );
        let a = run(dir.path(), &HashMap::new());
        assert!(a.reboot_required);
        assert!(!a.restart_required);
        assert_eq!(
            a.reasons,
            vec![Reason::RebootFlag {
                path: MARKER.to_string(),
                packages: vec![
                    "linux-image-6.8.0-40-generic".to_string(),
                    "linux-base".to_string()
                ],
                packages_truncated: false,
            }]
        );
    }

    #[test]
    fn marker_without_packages_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), MARKER, "");
        let a = run(dir.path(), &HashMap::new());
        assert!(a.reboot_required);
        assert_eq!(
            a.reasons,
            vec![Reason::RebootFlag {
                path: MARKER.to_string(),
                packages: vec![],
                packages_truncated: false,
            }]
        );
    }

    #[test]
    fn marker_packages_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), MARKER, "");
        let body: String = (0..PKGS_MAX_NAMES + 50)
            .map(|i| format!("pkg{i}\n"))
            .collect();
        write(dir.path(), MARKER_PKGS, &body);
        let a = run(dir.path(), &HashMap::new());
        match a.reasons.as_slice() {
            [Reason::RebootFlag {
                packages,
                packages_truncated,
                ..
            }] => {
                assert_eq!(packages.len(), PKGS_MAX_NAMES);
                assert!(*packages_truncated);
            }
            other => panic!("unexpected reasons: {other:?}"),
        }
    }

    #[test]
    fn kernel_running_tree_removed_requires_reboot() {
        let dir = tempfile::tempdir().unwrap();
        kernel(dir.path(), "6.8.0-1-arch1", &["6.9.2-arch1"]);
        let a = run(dir.path(), &HashMap::new());
        assert!(a.reboot_required);
        assert!(!a.restart_required);
        assert_eq!(
            a.reasons,
            vec![Reason::Kernel {
                running: "6.8.0-1-arch1".to_string(),
                installed: vec!["6.9.2-arch1".to_string()],
                reboot_required: true,
            }]
        );
    }

    #[test]
    fn kernel_other_release_also_installed_is_informational() {
        let dir = tempfile::tempdir().unwrap();
        kernel(dir.path(), "6.8.0", &["6.8.0", "6.9.0"]);
        let a = run(dir.path(), &HashMap::new());
        assert!(!a.reboot_required);
        assert_eq!(
            a.reasons,
            vec![Reason::Kernel {
                running: "6.8.0".to_string(),
                installed: vec!["6.8.0".to_string(), "6.9.0".to_string()],
                reboot_required: false,
            }]
        );
    }

    #[test]
    fn kernel_only_running_tree_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        kernel(dir.path(), "6.8.0", &["6.8.0"]);
        assert_eq!(run(dir.path(), &HashMap::new()), Assessment::default());
    }

    #[test]
    fn kernel_without_module_trees_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proc/sys/kernel/osrelease", "6.8.0\n");
        assert_eq!(run(dir.path(), &HashMap::new()), Assessment::default());
    }

    #[test]
    fn kernel_release_with_separator_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        kernel(dir.path(), "6.8.0", &["6.9.0"]);
        // The running value is overwritten by the bad one, so the kernel check is skipped.
        write(dir.path(), "proc/sys/kernel/osrelease", "../..\n");
        assert_eq!(run(dir.path(), &HashMap::new()), Assessment::default());
    }

    #[test]
    fn replaced_executable_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut links = HashMap::new();
        process(
            dir.path(),
            &mut links,
            101,
            "Hyprland\n",
            Some("/usr/bin/Hyprland (deleted)"),
            None,
        );
        let a = run(dir.path(), &links);
        assert!(!a.reboot_required);
        assert!(a.restart_required);
        assert_eq!(
            a.reasons,
            vec![Reason::ReplacedBinary {
                pid: 101,
                command: Some("Hyprland".to_string()),
                path: "/usr/bin/Hyprland".to_string(),
            }]
        );
    }

    #[test]
    fn replaced_libraries_from_maps_are_deduplicated_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let maps = "\
7f00-7f10 r-xp 00000000 08:01 1234 /usr/lib/libfoo.so.1 (deleted)
7f20-7f30 rw-s 00000000 00:01 99 /memfd:doubled (deleted)
7f40-7f50 rw-s 00000000 00:05 77 /dev/shm/pulse-shm-1 (deleted)
7f60-7f70 rw-s 00000000 00:04 55 /SYSV00000000 (deleted)
7f80-7f90 r--p 00000000 08:01 13 /usr/lib/libbar with space.so (deleted)
7fa0-7fb0 r--p 00000000 08:01 14 /usr/lib/libok.so
7fc0-7fd0 r-xp 00000000 08:01 15 /usr/bin/app (deleted)
";
        let mut links = HashMap::new();
        process(
            dir.path(),
            &mut links,
            7,
            "app\n",
            Some("/usr/bin/app (deleted)"),
            Some(maps),
        );
        let a = run(dir.path(), &links);
        let paths: Vec<&str> = a
            .reasons
            .iter()
            .filter_map(|r| match r {
                Reason::ReplacedBinary { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                "/usr/bin/app",
                "/usr/lib/libbar with space.so",
                "/usr/lib/libfoo.so.1",
            ]
        );
    }

    #[test]
    fn maps_pathname_parses_fields() {
        assert_eq!(
            maps_pathname("7f00-7f10 r-xp 00000000 08:01 1234    /usr/lib/a b.so (deleted)"),
            Some("/usr/lib/a b.so (deleted)")
        );
        assert_eq!(maps_pathname("7f00-7f10 rw-p 00000000 00:00 0 "), None);
        assert_eq!(maps_pathname("garbage"), None);
    }

    #[test]
    fn per_process_and_total_bounds_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let mut links = HashMap::new();
        let many: String = (0..DELETED_PER_PROCESS + 3)
            .map(|i| format!("7f00-7f01 r--p 0 08:01 {i} /usr/lib/lib{i}.so (deleted)\n"))
            .collect();
        process(
            dir.path(),
            &mut links,
            1,
            "many\n",
            None,
            Some(many.as_str()),
        );
        let a = run(dir.path(), &links);
        assert_eq!(a.reasons.len(), DELETED_PER_PROCESS);
        assert!(a.truncated);

        let dir = tempfile::tempdir().unwrap();
        let mut links = HashMap::new();
        for pid in 1..=(DELETED_TOTAL as u32 + 20) {
            process(
                dir.path(),
                &mut links,
                pid,
                "x\n",
                Some(format!("/usr/bin/x{pid} (deleted)").as_str()),
                None,
            );
        }
        let a = run(dir.path(), &links);
        assert_eq!(a.reasons.len(), DELETED_TOTAL);
        assert!(a.truncated);
    }

    #[test]
    fn unreadable_process_entries_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut links = HashMap::new();
        // pid 201: no exe link readable and no maps file: nothing to report.
        process(dir.path(), &mut links, 201, "quiet\n", None, None);
        // pid 202: exe unreadable, maps readable: the maps entry is still reported.
        process(
            dir.path(),
            &mut links,
            202,
            "partial\n",
            None,
            Some("7f00-7f01 r--p 0 08:01 5 /usr/lib/libz.so (deleted)\n"),
        );
        // pid 203: an unreadable maps file (a directory where the file should be).
        write(dir.path(), "proc/203/comm", "broken\n");
        fs::create_dir_all(dir.path().join("proc/203/maps")).unwrap();
        // Non-numeric entries under proc are not processes, even with a replaced exe.
        write(dir.path(), "proc/self/comm", "self\n");
        links.insert(
            dir.path().join("proc/self/exe"),
            PathBuf::from("/usr/bin/self (deleted)"),
        );

        let a = run(dir.path(), &links);
        assert_eq!(
            a.reasons,
            vec![Reason::ReplacedBinary {
                pid: 202,
                command: Some("partial".to_string()),
                path: "/usr/lib/libz.so".to_string(),
            }]
        );
        assert!(a.restart_required);
        assert!(!a.reboot_required);
    }

    #[test]
    fn all_reasons_together() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), MARKER, "");
        write(dir.path(), MARKER_PKGS, "linux-image-generic\n");
        kernel(dir.path(), "6.8.0-1-arch1", &["6.9.2-arch1"]);
        let mut links = HashMap::new();
        process(
            dir.path(),
            &mut links,
            101,
            "Hyprland\n",
            Some("/usr/bin/Hyprland (deleted)"),
            None,
        );
        let a = run(dir.path(), &links);
        assert!(a.reboot_required);
        assert!(a.restart_required);
        assert!(!a.truncated);
        let kinds: Vec<bool> = a.reasons.iter().map(Reason::needs_reboot).collect();
        assert_eq!(kinds, vec![true, true, false]);
        assert!(matches!(a.reasons[0], Reason::RebootFlag { .. }));
        assert!(matches!(a.reasons[1], Reason::Kernel { .. }));
        assert!(matches!(a.reasons[2], Reason::ReplacedBinary { .. }));
    }

    #[test]
    fn outcome_is_reboot_restart_or_none() {
        // Clean root: no reasons, so the outcome is none.
        let dir = tempfile::tempdir().unwrap();
        let a = run(dir.path(), &HashMap::new());
        assert_eq!(a.outcome, Outcome::None);

        // A replaced binary alone calls for a restart.
        let dir = tempfile::tempdir().unwrap();
        let mut links = HashMap::new();
        process(
            dir.path(),
            &mut links,
            101,
            "Hyprland\n",
            Some("/usr/bin/Hyprland (deleted)"),
            None,
        );
        assert_eq!(run(dir.path(), &links).outcome, Outcome::Restart);

        // A reboot flag outranks the replaced binary in the same tree.
        write(dir.path(), MARKER, "");
        assert_eq!(run(dir.path(), &links).outcome, Outcome::Reboot);

        // The running kernel tree removed calls for a reboot on its own.
        let dir = tempfile::tempdir().unwrap();
        kernel(dir.path(), "6.8.0-1-arch1", &["6.9.2-arch1"]);
        assert_eq!(run(dir.path(), &HashMap::new()).outcome, Outcome::Reboot);

        let value = serde_json::to_value(Outcome::Reboot).unwrap();
        assert_eq!(value, serde_json::json!("reboot"));
    }

    #[test]
    fn assessment_serialises_with_kind_tags() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), MARKER, "");
        let value = serde_json::to_value(assess(dir.path())).unwrap();
        assert_eq!(value["reboot_required"], serde_json::json!(true));
        assert_eq!(
            value["reasons"][0]["kind"],
            serde_json::json!("reboot_flag")
        );
    }
}
