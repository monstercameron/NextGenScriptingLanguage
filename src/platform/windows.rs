//! Native Windows handles, jobs, ACLs and cancellation. No WSL or implicit shell.
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::{
    ffi::OsStrExt,
    fs::OpenOptionsExt,
    io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock, Weak,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{Diagnostics::ToolHelp::*, JobObjects::*, Pipes::*, Threading::*, IO::*},
};

/// Add arguments with a command-line representation accepted by both the MSVC
/// CRT and MSYS-family parsers. Rust's default quoting handles spaces and tabs,
/// while MSYS also treats CR/LF/VT/FF as argument separators. Keep ordinary
/// arguments in `Command`'s regular path because tools such as cmd.exe have
/// additional command-line syntax beyond the CRT parser.
pub(crate) fn add_arguments<I, S>(command: &mut Command, arguments: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::process::CommandExt;

    for argument in arguments {
        let units: Vec<u16> = argument.as_ref().encode_wide().collect();
        if !units.iter().any(|unit| matches!(*unit, 0x0a..=0x0d)) {
            command.arg(argument);
            continue;
        }
        let mut quoted = Vec::with_capacity(units.len() + 2);
        quoted.push(b'"' as u16);
        let mut slashes = 0usize;
        for unit in units {
            match unit {
                x if x == b'\\' as u16 => slashes += 1,
                x if x == b'"' as u16 => {
                    quoted.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
                    quoted.push(x);
                    slashes = 0;
                }
                x => {
                    quoted.extend(std::iter::repeat_n(b'\\' as u16, slashes));
                    quoted.push(x);
                    slashes = 0;
                }
            }
        }
        quoted.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        quoted.push(b'"' as u16);
        command.raw_arg(std::ffi::OsString::from_wide(&quoted));
    }
}

pub(crate) fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
type Registration = (Weak<AtomicBool>, Option<Weak<std::sync::atomic::AtomicU8>>);

pub(crate) fn signals(
    cancel: Arc<AtomicBool>,
    drain: Option<Arc<std::sync::atomic::AtomicU8>>,
) -> Result<()> {
    static FLAGS: OnceLock<Mutex<Vec<Registration>>> = OnceLock::new();
    static INSTALLED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    FLAGS.get_or_init(|| Mutex::new(Vec::new()));
    let installed = INSTALLED.get_or_init(|| {
        ctrlc::set_handler(|| {
            super::record_cancel_signal(2);
            if let Some(flags) = FLAGS.get() {
                let mut flags = flags.lock().unwrap_or_else(|e| e.into_inner());
                flags.retain(|(cancel, _)| cancel.strong_count() != 0);
                for (cancel, drain) in flags.iter() {
                    // A run with an `on interrupt` body drains on its first interrupt.
                    if let Some(drain) = drain.as_ref().and_then(|weak| weak.upgrade()) {
                        if super::request_drain(&drain) {
                            continue;
                        }
                        drain.store(super::DRAIN_CANCELLED, Ordering::SeqCst);
                    }
                    if let Some(flag) = cancel.upgrade() {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
        })
        .map_err(|e| e.to_string())
    });
    if let Err(message) = installed {
        bail!("registering Windows console cancellation: {message}");
    }
    let mut flags = FLAGS
        .get()
        .unwrap()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    flags.retain(|(cancel, _)| cancel.strong_count() != 0);
    flags.push((Arc::downgrade(&cancel), drain.as_ref().map(Arc::downgrade)));
    Ok(())
}

/// `path` (absolute and normalized) in the verbatim `\\?\` form, without a NUL.
/// Rename-by-handle rejects plain drive and UNC paths with ERROR_INVALID_NAME.
fn verbatim_wide(path: &Path) -> Vec<u16> {
    let text: Vec<u16> = path.as_os_str().encode_wide().collect();
    let units = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    if text.starts_with(&units(r"\\?\")) || text.starts_with(&units(r"\\.\")) {
        return text;
    }
    if let Some(rest) = text.strip_prefix(units(r"\\").as_slice()) {
        let mut name = units(r"\\?\UNC\");
        name.extend_from_slice(rest);
        return name;
    }
    let mut name = units(r"\\?\");
    name.extend_from_slice(&text);
    name
}

/// Rename `source` over `target` with `FILE_RENAME_FLAG_POSIX_SEMANTICS`. Returns
/// `Ok(false)` when the file system or Windows version does not support it.
fn posix_replace(source: &Path, target: &Path) -> std::io::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    // From winbase.h (Win32_System_WindowsProgramming, a windows-sys feature not enabled).
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 2;
    let name = verbatim_wide(&std::path::absolute(target)?);
    let name_bytes = name.len() * std::mem::size_of::<u16>();
    let file = std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(source)?;
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    // The kernel reads FileName as a NUL-terminated path, not only FileNameLength bytes,
    // so the zeroed buffer must hold a terminator after the name; without it the new
    // name runs on into whatever follows the buffer.
    let size = (name_offset + name_bytes + std::mem::size_of::<u16>())
        .max(std::mem::size_of::<FILE_RENAME_INFO>());
    let (Ok(name_length), Ok(buffer_size)) = (u32::try_from(name_bytes), u32::try_from(size))
    else {
        return Ok(false);
    };
    // u64 storage keeps the buffer aligned for FILE_RENAME_INFO's HANDLE field.
    let mut buffer = vec![0_u64; size.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buffer` holds at least `size` zeroed bytes, aligned to 8, which covers the
    // FILE_RENAME_INFO header, `name_bytes` of file name starting at FileName and a zero
    // terminator after it. The
    // writes stay inside that allocation, and the handle stays open for the call.
    let renamed = unsafe {
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = name_length;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast_const().cast(),
            buffer_size,
        )
    };
    if renamed != 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_INVALID_PARAMETER: the
        // file system or this Windows version lacks POSIX rename semantics.
        // ERROR_INVALID_NAME (123): a path form the rename-by-handle call rejects.
        Some(1 | 50 | 87 | 123) => Ok(false),
        _ => Err(error),
    }
}

/// Open `path` as a directory handle for durability flushes. The open never
/// follows a trailing reparse point (`FILE_FLAG_OPEN_REPARSE_POINT`), and the
/// handle is verified to be a directory before use: a junction or symlink
/// parent is refused fail-closed instead of flushing some other directory's
/// metadata. `FlushFileBuffers` requires `GENERIC_WRITE`, so a parent the
/// caller cannot write fails here with access denied rather than at flush
/// time; checkpoint and journal directories must grant the writer write access
/// to their parent directory.
fn open_directory_no_reparse(path: &Path) -> std::io::Result<File> {
    let directory = OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    if !directory.metadata()?.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to flush {}: it is a reparse point or not a directory; checkpoint and journal directories must live in a real local directory",
                path.display()
            ),
        ));
    }
    Ok(directory)
}

/// Map a directory-flush failure fail-closed. Volumes without directory-flush
/// support (FAT/exFAT, some network redirectors) stay errors: the rename
/// already happened, so the caller still reports published-but-unsynced rather
/// than silently succeeding or re-running the publish. Only the message is
/// enriched, naming the likely volume limitation and the local-NTFS
/// requirement. There is deliberately no silent degraded mode: an unflushed
/// entry must stay visible as uncertainty.
fn map_flush_error(path: &Path, error: std::io::Error) -> std::io::Error {
    // ERROR_INVALID_FUNCTION (1), ERROR_NOT_SUPPORTED (50),
    // ERROR_INVALID_PARAMETER (87): the volume or redirector rejects the flush.
    match error.raw_os_error() {
        Some(1 | 50 | 87) => std::io::Error::new(
            error.kind(),
            format!(
                "directory flush is not supported for {path} ({error}); checkpoint and journal directories must live on a local NTFS volume",
                path = path.display()
            ),
        ),
        _ => error,
    }
}

/// Flush one open directory handle. `path` is only a label for error messages;
/// the flush addresses the handle, so a parent-path swap after the open cannot
/// redirect it.
fn flush_directory_handle(directory: &File, path: &Path) -> std::io::Result<()> {
    // SAFETY: `directory` is a live directory handle for this call, and
    // FlushFileBuffers only flushes that handle's metadata without copying any
    // bytes. A handle of the wrong type makes the call fail, not misbehave.
    if unsafe { FlushFileBuffers(directory.as_raw_handle()) } == 0 {
        return Err(map_flush_error(path, std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Ask the OS to persist a just-published rename's directory entry. The file's
/// contents were already synced before the rename; only the directory entry is
/// new. Directories open only with `FILE_FLAG_BACKUP_SEMANTICS`, never through
/// a trailing reparse point, and the handle is verified to be a directory, so
/// a junction parent fails instead of flushing another directory.
/// `FlushFileBuffers` requires the handle to have `GENERIC_WRITE` access, so
/// the directory opens for read and write; a read-only handle would fail the
/// flush. Success means the flush call succeeded, not that the entry provably
/// survives power loss: directory-entry persistence is filesystem dependent
/// (use a local NTFS directory), volumes without flush support fail closed
/// with an explicit error rather than degrading silently, and a parent swap
/// between the rename and a path-based flush is not pinned the way Unix
/// pinned-directory handles are (the publish backend pins the parent handle
/// before the rename to narrow this; see below). Hard power-loss survival is
/// proven only by the 50-trial hard-reset lab.
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    let directory = open_directory_no_reparse(path)?;
    flush_directory_handle(&directory, path)
}

/// Flush the parent directory of a just-published `target` so the rename entry
/// reaches disk. A bare file name publishes into the working directory.
fn flush_published(target: &Path) -> std::io::Result<()> {
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    sync_directory(parent)
}

/// Replace `target` with `source` without returning until the move is on disk.
/// A reader that holds the target open (a status poller, an antivirus scan)
/// makes the replace fail with access denied or a sharing violation for a
/// moment, so those two errors are retried with a short backoff before the
/// publish fails.
fn move_file_write_through(source: &Path, target: &Path) -> std::io::Result<()> {
    let (source, target) = (wide(source.as_os_str()), wide(target.as_os_str()));
    let mut attempt = 0;
    loop {
        // SAFETY: MoveFileExW reads two NUL-terminated UTF-16 paths. `wide` appends the
        // NUL, and `source` and `target` are locals that outlive the call.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } != 0
        {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        // ERROR_ACCESS_DENIED (5) and ERROR_SHARING_VIOLATION (32).
        if !matches!(error.raw_os_error(), Some(5 | 32)) || attempt == 5 {
            return Err(error);
        }
        std::thread::sleep(std::time::Duration::from_millis(5 << attempt));
        attempt += 1;
    }
}

/// The production Win32 backend for [`super::publish::windows_publish_with`].
///
/// The target's parent directory is opened before either rename attempt, and
/// the post-rename flush addresses that pinned handle rather than a path
/// re-resolved after the rename, so a parent-path swap between the rename and
/// the flush cannot redirect the flush to the wrong directory. The pin is
/// best-effort: when it is unavailable the flush resolves the parent by path,
/// which stays reparse-safe through [`open_directory_no_reparse`]. A failed
/// re-pin never clears an earlier good pin. The residual window is a swap
/// between the pin and the rename (and the `MoveFileExW` fallback, which only
/// takes paths, cannot pin at all); journal publishes hold the checkpoint lock
/// across the whole publish, excluding that swap for the checkpoint path.
struct NativePublishBackend {
    /// `(parent path, open handle)` pinned before the rename. The path is only
    /// a label for error messages; the flush addresses the handle.
    pinned_parent: Mutex<Option<(PathBuf, File)>>,
}

impl NativePublishBackend {
    fn new() -> Self {
        Self {
            pinned_parent: Mutex::new(None),
        }
    }

    /// Pin `target`'s parent directory before a rename attempt. Failures are
    /// ignored here so the publish still proceeds to a path-based flush; that
    /// fallback is itself reparse-safe, and a reparse-point parent fails there
    /// rather than flushing elsewhere.
    fn pin_parent(&self, target: &Path) {
        let parent = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if let Ok(directory) = open_directory_no_reparse(&parent) {
            let mut pinned = self
                .pinned_parent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *pinned = Some((parent, directory));
        }
    }
}

impl super::publish::WindowsPublishBackend for NativePublishBackend {
    fn posix_rename(&self, source: &Path, target: &Path) -> std::io::Result<bool> {
        self.pin_parent(target);
        posix_replace(source, target)
    }

    fn write_through_move(&self, source: &Path, target: &Path) -> std::io::Result<()> {
        self.pin_parent(target);
        move_file_write_through(source, target)
    }

    fn flush_published(&self, target: &Path) -> std::io::Result<()> {
        if let Some((parent, pinned)) = self
            .pinned_parent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            return flush_directory_handle(&pinned, &parent);
        }
        flush_published(target)
    }
}

pub(crate) fn replace_file(
    source: &Path,
    target: &Path,
) -> std::result::Result<(), super::publish::ReplaceError> {
    // A POSIX-semantics rename replaces the target even while a reader holds it open
    // with FILE_SHARE_DELETE (std's default share mode, and the checkpoint readers'),
    // which MoveFileExW refuses with access denied. File systems without it (FAT,
    // older Windows) fall back to MoveFileExW below.
    // The backend pins the target's parent directory before either rename so the
    // post-rename flush addresses that handle rather than a re-resolved path.
    // Both paths end with a flush of the pinned parent so the OS is asked to
    // persist the move; the fallback additionally requests
    // MOVEFILE_WRITE_THROUGH. Write-through is documented for copy-and-delete
    // moves, so same-volume rename persistence stays filesystem dependent and
    // unproven without the hard-reset lab. Any failure of the POSIX rename falls
    // through to MoveFileExW, which reports its own error if the replace really
    // cannot happen. A flush failure after a successful rename returns
    // `ReplaceError` with `published == true`: the target is published and the
    // caller must not report a failed publish. Volumes without flush support
    // fail closed with an explicit error; there is no silent degraded mode.
    super::publish::windows_publish_with(&NativePublishBackend::new(), source, target).map(|_| ())
}

pub(crate) struct Job(OwnedHandle);
impl Job {
    fn new() -> Result<Self> {
        // SAFETY: both arguments are null, which asks for default security and an unnamed
        // job. The returned handle is compared with null on the next line before use.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error()).context("creating child job");
        }
        // SAFETY: `raw` is a non-null job handle that this function just created and checked,
        // and nothing else holds it, so OwnedHandle takes sole ownership and closes it once.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a plain C struct of integers and
        // pointer-sized fields, so the all-zero bit pattern is a valid value. Only LimitFlags
        // is set below; the other limits stay zero, which means none.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `handle` owns the live job created above. The info class matches the struct
        // type, and `limits` is a local whose address and size_of_val are passed together, so
        // the kernel reads exactly one JOBOBJECT_EXTENDED_LIMIT_INFORMATION. Failure is
        // checked by the `== 0` test.
        if unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("configuring child job");
        }
        Ok(Self(handle))
    }
    pub(crate) fn terminate(&self) {
        // SAFETY: self.0 is the job handle owned by this Job. It stays open until Drop and
        // was created with full access, which includes JOB_OBJECT_TERMINATE. The return
        // value is ignored on purpose: if termination fails, the job still closes on drop
        // with KILL_ON_JOB_CLOSE (set in `new`), which ends the children.
        unsafe {
            TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
    pub(crate) fn spawn(command: &mut Command) -> Result<(Child, Self)> {
        use std::os::windows::process::CommandExt;
        let job = Self::new()?;
        // No untrusted child instruction runs until it belongs to our job.
        let mut child = command
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .context("spawning native Windows child")?;
        let setup = (|| -> Result<()> {
            // SAFETY: `job` and `child` are live for the whole closure. The child was created
            // suspended (CREATE_SUSPENDED above), so none of its code has run before it joins
            // the job, and the process handle from CreateProcess has the access this call needs.
            if unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), child.as_raw_handle()) }
                == 0
            {
                return Err(std::io::Error::last_os_error()).context("assigning child job");
            }
            let thread = suspended_primary_thread(&child)?;
            // SAFETY: `thread` is the suspended primary thread of `child`, as returned by
            // suspended_primary_thread. It was opened with THREAD_SUSPEND_RESUME and stays open
            // for this call. ResumeThread reports failure as u32::MAX, which the test checks.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(std::io::Error::last_os_error()).context("resuming child");
            }
            Ok(())
        })();
        if let Err(error) = setup {
            job.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok((child, job))
    }
}

// Undocumented but exported by every supported ntdll; enumerates only the
// threads of one process. Used by standard process inspection tools.
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtGetNextThread(
        process_handle: HANDLE,
        thread_handle: HANDLE,
        desired_access: u32,
        handle_attributes: u32,
        flags: u32,
        new_thread_handle: *mut HANDLE,
    ) -> i32;
}

const STATUS_NO_MORE_ENTRIES: i32 = 0x8000_001A_u32 as i32;

/// Primary thread of a child created suspended. A Toolhelp thread snapshot
/// lists every thread on the system and costs tens of milliseconds per launch,
/// so the child's own thread list is consulted first. That answer is accepted
/// only when it holds exactly one thread owned by the child, which is the
/// suspended primary thread; any other outcome uses the documented snapshot.
fn suspended_primary_thread(child: &Child) -> Result<OwnedHandle> {
    if let Some(thread) = process_only_primary_thread(child.as_raw_handle(), child.id()) {
        return Ok(thread);
    }
    snapshot_primary_thread(child.id())
}

fn process_only_primary_thread(process: HANDLE, pid: u32) -> Option<OwnedHandle> {
    let access = THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION;
    let mut first: HANDLE = std::ptr::null_mut();
    // SAFETY: the process handle is owned by the live child; the output
    // pointer refers to a local handle slot.
    let status =
        // SAFETY: `process` is the live child process handle, and a null thread argument asks
        // NtGetNextThread for the first thread of that process. `&mut first` is a local output
        // slot. NtGetNextThread is undocumented, so this relies on its observed behaviour, not
        // a published contract; the status and null checks below guard the returned handle.
        unsafe { NtGetNextThread(process, std::ptr::null_mut(), access, 0, 0, &mut first) };
    if status != 0 || first.is_null() {
        return None;
    }
    // SAFETY: a successful NtGetNextThread transfers one owned handle.
    let first = unsafe { OwnedHandle::from_raw_handle(first) };
    let mut second: HANDLE = std::ptr::null_mut();
    // SAFETY: `first` is an open thread handle with the access requested above.
    let next =
        // SAFETY: `first` is the thread handle returned by the call above (checked non-null),
        // used as the start point of the enumeration. `&mut second` is a local output slot.
        // The same undocumented-API caveat as the call above applies.
        unsafe { NtGetNextThread(process, first.as_raw_handle(), access, 0, 0, &mut second) };
    if next == 0 {
        // SAFETY: a successful call transferred a second owned handle.
        drop(unsafe { OwnedHandle::from_raw_handle(second) });
        return None;
    }
    if next != STATUS_NO_MORE_ENTRIES {
        return None;
    }
    // SAFETY: the thread handle is open and the query needs no output buffer.
    if unsafe { GetProcessIdOfThread(first.as_raw_handle()) } != pid {
        return None;
    }
    Some(first)
}

fn snapshot_primary_thread(pid: u32) -> Result<OwnedHandle> {
    // SAFETY: a thread snapshot takes no process argument (0 is ignored), and the returned
    // handle is compared with INVALID_HANDLE_VALUE on the next line before use.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .context("enumerating suspended primary thread");
    }
    // SAFETY: the snapshot is not INVALID_HANDLE_VALUE (the early return above handles
    // that), so it is an open handle. OwnedHandle takes sole ownership and closes it once.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: THREADENTRY32 is a plain struct of u32 fields, so all zeroes is a valid value.
    // dwSize is set on the next line, which Toolhelp requires before enumerating.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    // SAFETY: `snapshot` is the open snapshot handle owned above, and `entry` is a writable
    // THREADENTRY32 whose dwSize was set just before this call.
    let mut available = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while available != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: OpenThread takes a plain access mask, no inheritance and a thread id copied
            // from the snapshot entry. A null result (the thread has already exited) is checked.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(std::io::Error::last_os_error())
                    .context("opening suspended child thread");
            }
            // SAFETY: `thread` is non-null, since the branch above returned on null, so it is an
            // open handle that OpenThread created and OwnedHandle now owns.
            return Ok(unsafe { OwnedHandle::from_raw_handle(thread) });
        }
        // SAFETY: the same open snapshot and the entry initialised before the loop, which
        // Thread32Next updates in place. The loop continues only while `available` is nonzero.
        available = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    bail!("suspended primary thread not found")
}

/// Nonblocking observation of a pipe. Reading at most this many bytes cannot block;
/// BrokenPipe is an ordinary EOF. The borrowed handle keeps the pipe open for the call.
pub(crate) fn available(handle: BorrowedHandle<'_>) -> std::io::Result<Option<usize>> {
    let mut bytes = 0;
    let handle = handle.as_raw_handle() as HANDLE;
    // SAFETY: `handle` is open for the whole call because it is borrowed from a live Rust
    // handle, and it is a pipe handle because every caller passes a child pipe. PeekNamedPipe
    // with a null buffer and size 0 copies nothing and does not block, and `&mut bytes` is a
    // valid u32 out-slot. A handle of the wrong type makes it fail, not misbehave.
    if unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut bytes,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error().map(|n| n as u32),
            Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
        ) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(bytes as usize))
}

/// Duplicate a synchronous pipe handle for one bounded I/O operation. The borrowed source
/// stays open for the call, and the duplicate is an owned File that the caller joins
/// any thread using before returning.
pub(crate) fn duplicate_handle_file(handle: BorrowedHandle<'_>) -> Result<File> {
    let source = handle.as_raw_handle() as HANDLE;
    // SAFETY: GetCurrentProcess returns a pseudo-handle that is always valid and needs no
    // closing.
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate = std::ptr::null_mut();
    // SAFETY: `process` is the current-process pseudo-handle, `source` is open for the whole
    // call because it is borrowed from a live Rust handle, and `&mut duplicate` is a valid
    // out-slot. DUPLICATE_SAME_ACCESS keeps the source access. The BOOL result is checked for
    // zero below before `duplicate` is used.
    if unsafe {
        DuplicateHandle(
            process,
            source,
            process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("duplicating synchronous worker pipe handle");
    }
    // SAFETY: DuplicateHandle succeeded (the early return above handles failure), so
    // `duplicate` is a new open handle that nothing else owns. File takes sole ownership.
    Ok(unsafe { File::from_raw_handle(duplicate) })
}

/// Request cancellation of a blocking synchronous OS call on an owned thread.
/// The caller remains responsible for joining the thread.
pub(crate) fn cancel_synchronous_io<T>(worker: &std::thread::JoinHandle<T>) {
    // SAFETY: `worker` is a live JoinHandle, so its native thread handle stays valid until
    // the caller joins it. CancelSynchronousIo only requests cancellation of that thread's
    // pending synchronous I/O; it does not free or close anything.
    unsafe {
        CancelSynchronousIo(worker.as_raw_handle());
    }
}

pub(crate) fn user_sid(process: HANDLE) -> Result<Vec<u8>> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `process` must be an open process handle that allows TOKEN_QUERY. The function
    // does not check that, and a bad handle only yields the error path. `token` is a local
    // out-slot, and the failure branch returns before the handle is used.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading process user");
    }
    // SAFETY: OpenProcessToken succeeded (the failure branch returned above), so `token` is a
    // new open handle that nothing else owns. OwnedHandle closes it once on drop.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    // SAFETY: a null buffer with length 0 is the documented size query. The call fails with
    // ERROR_INSUFFICIENT_BUFFER and writes the required length to `size`, which the next
    // call uses. The return value is intentionally not checked here; the second call is.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut size,
        );
    }
    let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: `data` holds at least `size` bytes (div_ceil rounds up to whole usize units),
    // and a Vec<usize> is aligned for TOKEN_USER. `size` is the length the query reported,
    // so the call writes only inside the buffer; `token` is still open here.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("reading process user SID");
    }
    // SAFETY: the successful TokenUser query above wrote a TOKEN_USER at the start of `data`,
    // which is usize-aligned. The reference borrows `data`, and `data` is neither moved nor
    // resized while it is used.
    let user = unsafe { &*(data.as_ptr().cast::<TOKEN_USER>()) };
    // SAFETY: `User.Sid` points at the SID that GetTokenInformation wrote into `data`, which
    // is still alive, so GetLengthSid reads only memory that the query filled.
    let length = unsafe { GetLengthSid(user.User.Sid) };
    let mut sid = vec![0u8; length as usize];
    // SAFETY: `sid` has exactly `length` bytes, the size GetLengthSid reported for the source
    // SID in the live `data` buffer. The two allocations do not overlap.
    if unsafe { CopySid(length, sid.as_mut_ptr().cast(), user.User.Sid) } == 0 {
        return Err(std::io::Error::last_os_error()).context("copying user SID");
    }
    Ok(sid)
}
pub(crate) struct PrivateSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}
/// SECURITY_ATTRIBUTES whose descriptor pointer is owned by a PrivateSecurity. The
/// lifetime `'a` ties every use to that owner, so the pointer cannot dangle.
pub(crate) struct SecurityAttributes<'a> {
    raw: SECURITY_ATTRIBUTES,
    _descriptor: std::marker::PhantomData<&'a PrivateSecurity>,
}
impl SecurityAttributes<'_> {
    /// A pointer to the attributes, valid while this value is borrowed. Callers pass it to
    /// a Win32 call that reads it during that call.
    pub(crate) fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.raw
    }
}
impl PrivateSecurity {
    pub(crate) fn new() -> Result<Self> {
        // SAFETY: GetCurrentProcess returns the current-process pseudo-handle, which is always
        // valid and needs no closing. user_sid only queries that process's token.
        let sid = user_sid(unsafe { GetCurrentProcess() })?;
        let mut text = std::ptr::null_mut();
        // SAFETY: `sid` is the valid SID copied by user_sid and is still alive during the call.
        // On success `text` is a LocalAlloc'd, NUL-terminated string that is freed below; the
        // failure case is the `== 0` branch.
        if unsafe { ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut text) } == 0 {
            return Err(std::io::Error::last_os_error()).context("encoding user SID");
        }
        let mut length = 0;
        // SAFETY: `text` is the non-null NUL-terminated string from ConvertSidToStringSidW, so
        // every index before the terminator is inside its allocation.
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the `length` units before the terminator lie inside the string allocation, and
        // the slice is copied into an owned String before LocalFree runs below.
        let sid_text = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })?;
        // SAFETY: `text` came from ConvertSidToStringSidW, which allocates it with LocalAlloc, so
        // LocalFree is its matching release. It is freed once and not read afterwards.
        unsafe {
            LocalFree(text.cast());
        }
        let sddl = wide(OsStr::new(&format!(
            "O:{sid_text}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid_text})"
        )));
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `sddl` is the NUL-terminated UTF-16 string built by `wide` above, and
        // SDDL_REVISION_1 is the only defined revision. On success `descriptor` is a LocalAlloc'd
        // security descriptor that PrivateSecurity owns and frees in Drop.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("building owner-only security descriptor");
        }
        Ok(Self { descriptor })
    }
    /// Attributes for secure object creation. They borrow this descriptor, so the lifetime stops
    /// them from outliving the PrivateSecurity whose memory they point into.
    pub(crate) fn attributes(&self) -> SecurityAttributes<'_> {
        SecurityAttributes {
            raw: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor,
                bInheritHandle: 0,
            },
            _descriptor: std::marker::PhantomData,
        }
    }
    fn apply(&self, file: &File) -> Result<()> {
        let (mut present, mut defaulted, mut dacl) = (0, 0, std::ptr::null_mut());
        // SAFETY: `self.descriptor` is the live descriptor from `new`, freed only in Drop, so it
        // outlives this call. The DACL pointer is an output into that descriptor and is used only
        // while `self` is borrowed; `present` is checked before the DACL is used.
        if unsafe {
            GetSecurityDescriptorDacl(self.descriptor, &mut present, &mut dacl, &mut defaulted)
        } == 0
            || present == 0
        {
            bail!("private DACL is unavailable");
        }
        // SAFETY: `file` is a live File, so its handle is a valid file object for SE_FILE_OBJECT.
        // `dacl` points into the live descriptor (see above) and SetSecurityInfo only reads it.
        // Missing WRITE_DAC access returns an error code, which is checked below.
        let error = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error as i32))
                .context("restricting checkpoint to current user");
        }
        Ok(())
    }
}

/// Create a file with a protected owner-only DACL and an explicit current-user owner.
/// Supplying the descriptor to `CreateFileW` makes the privacy boundary atomic with
/// creation, before another local principal can open the new file.
pub(crate) fn create_private_file(path: &Path) -> Result<File> {
    let security = PrivateSecurity::new()?;
    let attributes = security.attributes();
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and remains live for the call. The security
    // attributes borrow `security`, which remains live through the call. CreateFileW
    // either returns a valid file handle or INVALID_HANDLE_VALUE, checked below.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            attributes.as_ptr(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("creating private file {}", path.display()));
    }
    // SAFETY: CreateFileW returned a live handle and nothing else owns it; File takes
    // sole responsibility for closing it.
    Ok(unsafe { File::from_raw_handle(raw) })
}

impl Drop for PrivateSecurity {
    fn drop(&mut self) {
        // SAFETY: `descriptor` was allocated with LocalAlloc by the conversion in `new` and is
        // freed only here. PrivateSecurity is not Clone and is dropped once, so no other owner
        // exists. Copies handed out by `attributes` must not be used after this point.
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub(crate) fn restrict_file(file: &File) -> Result<()> {
    PrivateSecurity::new()?.apply(file)
}
pub(crate) fn restrict_directory(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .access_mode(WRITE_DAC | READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .with_context(|| format!("opening checkpoint directory {}", path.display()))?;
    if is_reparse(&file.metadata()?) {
        bail!("checkpoint directory cannot be a reparse point");
    }
    restrict_file(&file)
}
pub(crate) fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
pub(crate) fn directory_identity(path: &Path) -> Result<(u32, u64)> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // SAFETY: BY_HANDLE_FILE_INFORMATION is a plain struct of integer fields, so the all-zero
    // bit pattern is valid. GetFileInformationByHandle fills it in before any field is read.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` is alive for this call and was opened with FILE_READ_ATTRIBUTES, and
    // `info` is a writable out-struct of the type the API fills. The file was opened with
    // FILE_FLAG_OPEN_REPARSE_POINT, so the attributes describe the link itself.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading directory identity");
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("checkpoint directory cannot be a reparse point");
    }
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}
pub(crate) fn lock(file: &File) -> Result<()> {
    // SAFETY: OVERLAPPED is a plain C struct, so the all-zero bit pattern is valid. A zero
    // offset is the start of the file, which the whole-file range below begins from.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: `file` is a live File, and `overlapped` is a local that stays in scope for this
    // synchronous call. The range u32::MAX..u32::MAX locks the whole file. LockFileEx needs
    // a handle with GENERIC_READ or GENERIC_WRITE; otherwise it fails and the error returns.
    if unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("checkpoint is already locked or lock failed");
    }
    Ok(())
}

/// One bounded reader for an inherited console, file or pipe. Shutdown cancels
/// this exact worker; no thread is detached or allowed to fill a queue.
pub(crate) struct Pump {
    receiver: Option<std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    /// A chunk error received by `wait_readable`, returned by the next `read`.
    pending_error: Option<std::io::Error>,
    worker: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    pending: Vec<u8>,
    offset: usize,
    eof: bool,
}
impl Pump {
    pub(crate) fn new(mut reader: impl std::io::Read + Send + 'static) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let mut bytes = vec![0u8; 8192];
            let message = match reader.read(&mut bytes) {
                Ok(count) => {
                    bytes.truncate(count);
                    Ok(bytes)
                }
                Err(e) => Err(e),
            };
            let done = message.as_ref().map_or(true, |v| v.is_empty());
            let mut message = message;
            loop {
                if stopped.load(Ordering::Relaxed) {
                    return;
                }
                match sender.try_send(message) {
                    Ok(()) => break,
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
                    Err(std::sync::mpsc::TrySendError::Full(value)) => {
                        message = value;
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
            if done {
                break;
            }
        });
        Self {
            receiver: Some(receiver),
            pending_error: None,
            worker: Some(worker),
            stop,
            pending: Vec::new(),
            offset: 0,
            eof: false,
        }
    }
}
impl Pump {
    /// Block for at most `timeout` until a chunk arrives, without consuming it.
    /// Returns at once when bytes are buffered, an error is held, or the stream
    /// has ended, so a caller can wake on output instead of sleeping.
    pub(crate) fn wait_readable(&mut self, timeout: std::time::Duration) {
        if self.offset < self.pending.len() || self.eof || self.pending_error.is_some() {
            return;
        }
        let received = match self.receiver.as_ref() {
            Some(receiver) => receiver.recv_timeout(timeout),
            None => Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Ok(bytes)) => {
                self.eof = bytes.is_empty();
                self.pending = bytes;
                self.offset = 0;
            }
            Ok(Err(error)) => self.pending_error = Some(error),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.eof = true,
        }
    }
}

impl std::io::Read for Pump {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.offset == self.pending.len() {
            if self.eof {
                return Ok(0);
            }
            if let Some(error) = self.pending_error.take() {
                return Err(error);
            }
            let Some(receiver) = self.receiver.as_ref() else {
                self.eof = true;
                return Ok(0);
            };
            match receiver.try_recv() {
                Ok(result) => {
                    self.pending = result?;
                    self.offset = 0;
                    self.eof = self.pending.is_empty();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return Err(std::io::ErrorKind::WouldBlock.into());
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.eof = true;
                    return Ok(0);
                }
            }
        }
        let count = buffer.len().min(self.pending.len() - self.offset);
        buffer[..count].copy_from_slice(&self.pending[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}
impl Pump {
    /// Stop the bounded reader and join its blocking OS read. Dropping the
    /// receiver first ensures a full channel cannot strand the sender thread.
    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.receiver.take());
        if let Some(worker) = self.worker.take() {
            while !worker.is_finished() {
                // SAFETY: `worker` is not joined until after this loop, so its handle is valid.
                // Cancel repeats each millisecond until the thread ends, which also covers an I/O
                // call that starts after an earlier cancel.
                unsafe {
                    CancelSynchronousIo(worker.as_raw_handle());
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let _ = worker.join();
        }
    }
}
impl Drop for Pump {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;

    fn snapshot_owned_thread_ids(pid: u32) -> Vec<u32> {
        // SAFETY: a thread snapshot takes no process argument (0 is ignored), and the handle is
        // checked against INVALID_HANDLE_VALUE by the assert on the next line.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        assert_ne!(snapshot, INVALID_HANDLE_VALUE);
        // SAFETY: the assert above panics unless `snapshot` is valid, so OwnedHandle owns an
        // open handle and closes it once.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        // SAFETY: THREADENTRY32 is a plain u32 struct, so all zeroes is a valid value. dwSize is
        // set on the next line, as Toolhelp requires before enumeration.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut ids = Vec::new();
        // SAFETY: `snapshot` is open and `entry` is writable, with dwSize set above as Toolhelp
        // requires before the first Thread32First call.
        let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                ids.push(entry.th32ThreadID);
            }
            // SAFETY: the same open snapshot and initialised entry as the Thread32First call
            // above; the loop continues only while the previous call returned nonzero.
            more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
        }
        ids
    }

    #[test]
    fn process_only_lookup_agrees_with_snapshot_for_a_suspended_child() {
        let mut child = Command::new("cmd")
            .args(["/C", "exit 0"])
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .expect("spawning suspended child");
        let fast = process_only_primary_thread(child.as_raw_handle(), child.id())
            .expect("per-process thread lookup");
        // Owners from the system snapshot, the same source the fallback uses.
        let owned = snapshot_owned_thread_ids(child.id());
        assert_eq!(owned.len(), 1, "a suspended child has one thread");
        assert_eq!(
            // SAFETY: `fast` is the thread handle from process_only_primary_thread, which
            // requested THREAD_QUERY_LIMITED_INFORMATION, the access GetThreadId needs.
            unsafe { GetThreadId(fast.as_raw_handle()) },
            owned[0],
            "per-process lookup must name the suspended primary thread"
        );
        // SAFETY: `fast` was opened with THREAD_SUSPEND_RESUME by the lookup under test, and the
        // child was created with CREATE_SUSPENDED, so the previous suspend count is 1.
        assert_eq!(unsafe { ResumeThread(fast.as_raw_handle()) }, 1);
        assert!(child.wait().expect("waiting for child").success());
    }

    #[test]
    fn published_directory_flush_reaches_a_temporary_directory() {
        // `FlushFileBuffers` requires `GENERIC_WRITE` access, so this fails on
        // a read-only directory handle and passes with the read/write opener
        // above. It runs in the Windows `--lib` jobs. Success proves the
        // directory handle opens and the flush call succeeds, not that the
        // entry provably survives power loss.
        let temp = tempfile::tempdir().expect("temporary directory");
        super::sync_directory(temp.path()).expect("flushing a directory");
    }

    #[test]
    fn replace_file_publishes_bytes_and_flushes_parent() {
        // This asserts the rename happened and the follow-up flush call
        // succeeded — not that the entry provably survives power loss. Only
        // the 50-trial hard-reset lab proves that.
        let temp = tempfile::tempdir().expect("temporary directory");
        let source = temp.path().join("journal.json.tmp");
        let target = temp.path().join("journal.json");
        std::fs::write(&source, b"begin record").expect("staging a journal file");
        super::replace_file(&source, &target).expect("publishing a journal file");
        assert_eq!(
            std::fs::read(&target).expect("reading the published file"),
            b"begin record"
        );
    }
}
