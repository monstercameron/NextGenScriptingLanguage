//! Process lookup by exact name and uid under a proc root (#346).
//!
//! Everything here is read-only. The root is `/proc` in production and a
//! temporary fixture tree in tests, so the parsing runs on any platform.
//!
//! Matching rules, for `find`:
//! - A name of at most 15 bytes matches the kernel command name (`comm`) exactly.
//!   `comm` is the program name truncated by the kernel to 15 bytes, so a longer
//!   program name never equals its `comm`.
//! - A name longer than 15 bytes matches the basename of the first argument in
//!   `cmdline` (bounded read). A process that rewrites its own argv can change
//!   what this reports; that is a property of the process, not of this module.
//! - The uid must equal the real uid (first number of the `Uid:` line).

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Kernel command names are truncated to this many bytes (TASK_COMM_LEN - 1).
pub const COMM_MAX_BYTES: usize = 15;

/// Upper bound on a `stat` read. A stat line is a few hundred bytes.
const STAT_READ_MAX: u64 = 4096;
/// Upper bound on a `status` read. The `Uid:` line sits near the top.
const STATUS_READ_MAX: u64 = 8192;
/// Upper bound on a `cmdline` read. Only the first argument is needed.
const CMDLINE_READ_MAX: u64 = 4096;

/// A process reference that stays meaningful across pid reuse: the pid plus the
/// kernel start time (clock ticks since boot). Re-check it with
/// [`identity_matches`] immediately before signalling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

/// One process as read from the proc root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    /// Real uid from the `Uid:` line of `status`.
    pub uid: u32,
    /// Kernel command name from `stat`, at most 15 bytes (lossy UTF-8).
    pub comm: String,
    /// Start time in clock ticks since boot (field 22 of `stat`).
    pub start_time: u64,
    /// Target of `exe`, when readable (it is not for other users' processes
    /// without privilege, or for kernel threads).
    pub exe: Option<PathBuf>,
}

impl ProcessEntry {
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            start_time: self.start_time,
        }
    }
}

/// Every process under `proc_root`. A process that vanishes while it is read is
/// skipped. A missing or unreadable root gives an empty list.
pub fn list(proc_root: &Path) -> Vec<ProcessEntry> {
    pid_dirs(proc_root)
        .into_iter()
        .filter_map(|pid| read_core(proc_root, pid))
        .map(|entry| with_exe(proc_root, entry))
        .collect()
}

/// Processes whose matching name equals `exact_name` and whose real uid is `uid`.
/// Matching rules are in the module comment. An empty name matches nothing.
pub fn find(proc_root: &Path, exact_name: &str, uid: u32) -> Vec<ProcessEntry> {
    if exact_name.is_empty() {
        return Vec::new();
    }
    let long_name = exact_name.len() > COMM_MAX_BYTES;
    pid_dirs(proc_root)
        .into_iter()
        .filter_map(|pid| {
            let entry = read_core(proc_root, pid)?;
            if entry.uid != uid {
                return None;
            }
            let matched = if long_name {
                argv0_basename(proc_root, pid).is_some_and(|base| base == exact_name.as_bytes())
            } else {
                entry.comm == exact_name
            };
            matched.then(|| with_exe(proc_root, entry))
        })
        .collect()
}

/// True when the process with this pid still has the recorded start time.
/// Use it right before a signal. Pid reuse changes the start time, so a false
/// result means the pid now names a different process. A gap between this check
/// and the signal remains until a pidfd is used (see NEEDS in the report).
pub fn identity_matches(proc_root: &Path, identity: ProcessIdentity) -> bool {
    read_core(proc_root, identity.pid).is_some_and(|e| e.start_time == identity.start_time)
}

/// Numeric entries of the proc root, as pids. Non-numeric names such as `self`,
/// `sys` or `net` are not processes and are skipped. Pid 0 is not a process.
fn pid_dirs(proc_root: &Path) -> Vec<u32> {
    let Ok(read_dir) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    read_dir
        .filter_map(|dirent| dirent.ok())
        .filter_map(|dirent| parse_pid(&dirent.file_name()))
        .collect()
}

fn parse_pid(name: &OsStr) -> Option<u32> {
    let text = name.to_str()?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok().filter(|&pid| pid != 0)
}

/// Reads `stat` and `status`. Returns None when either is missing or unparsable,
/// which covers a process that exited during the read. `exe` is left empty.
fn read_core(proc_root: &Path, pid: u32) -> Option<ProcessEntry> {
    let dir = proc_root.join(pid.to_string());
    let stat = parse_stat(&read_bounded(&dir.join("stat"), STAT_READ_MAX)?)?;
    let uid = parse_uid(&read_bounded(&dir.join("status"), STATUS_READ_MAX)?)?;
    Some(ProcessEntry {
        pid,
        ppid: stat.ppid,
        uid,
        comm: stat.comm,
        start_time: stat.start_time,
        exe: None,
    })
}

fn with_exe(proc_root: &Path, mut entry: ProcessEntry) -> ProcessEntry {
    entry.exe = fs::read_link(proc_root.join(entry.pid.to_string()).join("exe")).ok();
    entry
}

struct Stat {
    comm: String,
    ppid: u32,
    start_time: u64,
}

/// `pid (comm) state ppid ...`. The command name may contain spaces and
/// parentheses, so everything after the LAST ')' is positional.
fn parse_stat(bytes: &[u8]) -> Option<Stat> {
    let open = bytes.iter().position(|&b| b == b'(')?;
    let close = bytes.iter().rposition(|&b| b == b')')?;
    if close <= open {
        return None;
    }
    let comm = String::from_utf8_lossy(&bytes[open + 1..close]).into_owned();
    let rest = std::str::from_utf8(bytes.get(close + 1..)?).ok()?;
    // rest begins at field 3 (state). Field k is index k - 3.
    let mut fields = rest.split_ascii_whitespace();
    let ppid = fields.nth(1)?.parse::<u32>().ok()?; // field 4
    let start_time = fields.nth(17)?.parse::<u64>().ok()?; // field 22
    Some(Stat {
        comm,
        ppid,
        start_time,
    })
}

/// First number on the `Uid:` line (real uid).
fn parse_uid(bytes: &[u8]) -> Option<u32> {
    for line in bytes.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"Uid:") {
            let first = rest
                .split(|b| b.is_ascii_whitespace())
                .find(|field| !field.is_empty())?;
            return std::str::from_utf8(first).ok()?.parse::<u32>().ok();
        }
    }
    None
}

/// Basename of the first NUL-terminated argument in `cmdline`, as raw bytes.
/// Kernel threads have an empty `cmdline` and give None.
fn argv0_basename(proc_root: &Path, pid: u32) -> Option<Vec<u8>> {
    let bytes = read_bounded(
        &proc_root.join(pid.to_string()).join("cmdline"),
        CMDLINE_READ_MAX,
    )?;
    let argv0 = bytes.split(|&b| b == 0).next()?;
    let base = argv0.rsplit(|&b| b == b'/').next()?;
    if base.is_empty() {
        return None;
    }
    Some(base.to_vec())
}

/// Reads at most `max` bytes. Any error, including a vanished process, is None.
fn read_bounded(path: &Path, max: u64) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat_bytes(pid: u32, comm: &str, ppid: u32, start: u64) -> Vec<u8> {
        format!("{pid} ({comm}) S {ppid} 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0\n")
            .into_bytes()
    }

    fn status_bytes(uid: u32) -> Vec<u8> {
        format!(
            "Name:\tfake\nState:\tS (sleeping)\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t{uid}\n"
        )
        .into_bytes()
    }

    /// Writes one fake process. `cmdline` is written verbatim.
    fn add_proc(
        root: &Path,
        pid: u32,
        comm: &str,
        ppid: u32,
        uid: u32,
        start: u64,
        cmdline: &[u8],
    ) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stat"), stat_bytes(pid, comm, ppid, start)).unwrap();
        fs::write(dir.join("status"), status_bytes(uid)).unwrap();
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    fn pids(entries: &[ProcessEntry]) -> Vec<u32> {
        let mut out: Vec<u32> = entries.iter().map(|e| e.pid).collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn ordinary_process_is_listed_and_found() {
        let dir = tempfile::tempdir().unwrap();
        add_proc(
            dir.path(),
            100,
            "sleeper",
            1,
            1000,
            555,
            b"/usr/bin/sleeper\0-n\0",
        );

        let all = list(dir.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].pid, 100);
        assert_eq!(all[0].ppid, 1);
        assert_eq!(all[0].uid, 1000);
        assert_eq!(all[0].comm, "sleeper");
        assert_eq!(all[0].start_time, 555);
        assert_eq!(
            all[0].identity(),
            ProcessIdentity {
                pid: 100,
                start_time: 555
            }
        );

        let found = find(dir.path(), "sleeper", 1000);
        assert_eq!(pids(&found), vec![100]);
        assert!(
            find(dir.path(), "sleep", 1000).is_empty(),
            "prefix must not match"
        );
        assert!(find(dir.path(), "", 1000).is_empty());
    }

    #[test]
    fn comm_with_parentheses_and_spaces_is_split_at_last_paren() {
        let dir = tempfile::tempdir().unwrap();
        add_proc(dir.path(), 101, "a) (b c", 7, 1000, 9, b"");

        let all = list(dir.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].comm, "a) (b c");
        assert_eq!(all[0].ppid, 7);
        assert_eq!(all[0].start_time, 9);
        assert_eq!(pids(&find(dir.path(), "a) (b c", 1000)), vec![101]);
    }

    #[test]
    fn truncated_comm_matches_long_name_through_cmdline() {
        let dir = tempfile::tempdir().unwrap();
        // The kernel stored only the first 15 bytes of the program name.
        add_proc(
            dir.path(),
            200,
            "abcdefghijklmno",
            1,
            1000,
            10,
            b"/opt/abcdefghijklmnopqrs\0--flag\0",
        );
        // Same comm, but argv0 is a different program: must not match the long name.
        add_proc(
            dir.path(),
            201,
            "abcdefghijklmno",
            1,
            1000,
            11,
            b"/usr/bin/other\0",
        );

        let long = find(dir.path(), "abcdefghijklmnopqrs", 1000);
        assert_eq!(pids(&long), vec![200]);

        // A 15-byte name is matched against comm, so both processes match it.
        let short = find(dir.path(), "abcdefghijklmno", 1000);
        assert_eq!(pids(&short), vec![200, 201]);
    }

    #[test]
    fn same_name_under_different_uids_is_split_by_uid() {
        let dir = tempfile::tempdir().unwrap();
        add_proc(dir.path(), 300, "worker", 1, 1000, 1, b"worker\0");
        add_proc(dir.path(), 301, "worker", 1, 0, 2, b"worker\0");

        assert_eq!(pids(&find(dir.path(), "worker", 1000)), vec![300]);
        assert_eq!(pids(&find(dir.path(), "worker", 0)), vec![301]);
        assert!(find(dir.path(), "worker", 4242).is_empty());
        assert_eq!(pids(&list(dir.path())), vec![300, 301]);
    }

    #[test]
    fn kernel_thread_with_empty_cmdline_matches_by_comm_only() {
        let dir = tempfile::tempdir().unwrap();
        add_proc(dir.path(), 2, "kthreadd", 0, 0, 3, b"");

        assert_eq!(pids(&find(dir.path(), "kthreadd", 0)), vec![2]);
        assert!(find(dir.path(), "kthreadd-with-a-long-name", 0).is_empty());
        let all = list(dir.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].ppid, 0);
    }

    #[test]
    fn vanished_process_with_stat_but_no_status_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        add_proc(dir.path(), 400, "ghost", 1, 1000, 77, b"ghost\0");
        fs::remove_file(dir.path().join("400").join("status")).unwrap();
        add_proc(dir.path(), 401, "live", 1, 1000, 78, b"live\0");

        assert_eq!(pids(&list(dir.path())), vec![401]);
        assert!(find(dir.path(), "ghost", 1000).is_empty());
        assert_eq!(pids(&find(dir.path(), "live", 1000)), vec![401]);
    }

    #[test]
    fn non_numeric_directories_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        add_proc(dir.path(), 500, "real", 1, 1000, 5, b"real\0");
        // Non-numeric names that hold a complete fake process: must be ignored.
        for name in ["self", "sys", "net", "12a"] {
            let sub = dir.path().join(name);
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("stat"), stat_bytes(999, "fake", 1, 1)).unwrap();
            fs::write(sub.join("status"), status_bytes(1000)).unwrap();
        }

        assert_eq!(pids(&list(dir.path())), vec![500]);
        assert!(find(dir.path(), "fake", 1000).is_empty());
    }

    #[test]
    fn missing_root_gives_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("no-such-proc");
        assert!(list(&absent).is_empty());
        assert!(find(&absent, "anything", 0).is_empty());
    }

    #[test]
    fn identity_detects_pid_reuse() {
        let dir = tempfile::tempdir().unwrap();
        add_proc(dir.path(), 600, "svc", 1, 1000, 42, b"svc\0");
        let id = ProcessIdentity {
            pid: 600,
            start_time: 42,
        };

        assert!(identity_matches(dir.path(), id));
        assert!(!identity_matches(
            dir.path(),
            ProcessIdentity {
                pid: 600,
                start_time: 43
            }
        ));
        assert!(!identity_matches(
            dir.path(),
            ProcessIdentity {
                pid: 601,
                start_time: 42
            }
        ));

        // The pid is reused by a different process with a new start time.
        add_proc(dir.path(), 600, "other", 1, 1000, 99, b"other\0");
        assert!(!identity_matches(dir.path(), id));
    }

    #[test]
    fn parse_uid_takes_first_number_of_uid_line() {
        assert_eq!(parse_uid(b"Uid:\t1000\t0\t0\t0\n"), Some(1000));
        assert_eq!(parse_uid(b"Name:\tx\nUid:  7 7 7 7\n"), Some(7));
        assert_eq!(parse_uid(b"Name:\tx\n"), None);
    }
}
