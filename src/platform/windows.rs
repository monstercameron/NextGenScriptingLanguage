//! Native Windows handles, jobs, ACLs and cancellation. No WSL or implicit shell.
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::{
    ffi::OsStrExt,
    fs::OpenOptionsExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use std::path::Path;
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

pub(crate) fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    let (source, target) = (wide(source.as_os_str()), wide(target.as_os_str()));
    // Both buffers remain alive and NUL-terminated for this call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct Job(OwnedHandle);
impl Job {
    fn new() -> Result<Self> {
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error()).context("creating child job");
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
            if unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), child.as_raw_handle()) }
                == 0
            {
                return Err(std::io::Error::last_os_error()).context("assigning child job");
            }
            let thread = suspended_primary_thread(&child)?;
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
        unsafe { NtGetNextThread(process, std::ptr::null_mut(), access, 0, 0, &mut first) };
    if status != 0 || first.is_null() {
        return None;
    }
    // SAFETY: a successful NtGetNextThread transfers one owned handle.
    let first = unsafe { OwnedHandle::from_raw_handle(first) };
    let mut second: HANDLE = std::ptr::null_mut();
    // SAFETY: `first` is an open thread handle with the access requested above.
    let next =
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
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .context("enumerating suspended primary thread");
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    let mut available = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while available != 0 {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(std::io::Error::last_os_error())
                    .context("opening suspended child thread");
            }
            return Ok(unsafe { OwnedHandle::from_raw_handle(thread) });
        }
        available = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    bail!("suspended primary thread not found")
}

/// Nonblocking observation of an owned anonymous/named pipe. Reading at most
/// this many bytes cannot block; BrokenPipe is an ordinary EOF.
pub(crate) fn available(handle: HANDLE) -> std::io::Result<Option<usize>> {
    let mut bytes = 0;
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

/// Duplicate a synchronous pipe handle for one bounded I/O operation. The
/// caller owns and joins any thread using the duplicate before returning.
pub(crate) fn duplicate_handle_file(handle: HANDLE) -> Result<File> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            process,
            handle,
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
    Ok(unsafe { File::from_raw_handle(duplicate) })
}

/// Request cancellation of a blocking synchronous OS call on an owned thread.
/// The caller remains responsible for joining the thread.
pub(crate) fn cancel_synchronous_io<T>(worker: &std::thread::JoinHandle<T>) {
    unsafe {
        CancelSynchronousIo(worker.as_raw_handle());
    }
}

pub(crate) fn user_sid(process: HANDLE) -> Result<Vec<u8>> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading process user");
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
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
    let user = unsafe { &*(data.as_ptr().cast::<TOKEN_USER>()) };
    let length = unsafe { GetLengthSid(user.User.Sid) };
    let mut sid = vec![0u8; length as usize];
    if unsafe { CopySid(length, sid.as_mut_ptr().cast(), user.User.Sid) } == 0 {
        return Err(std::io::Error::last_os_error()).context("copying user SID");
    }
    Ok(sid)
}
pub(crate) struct PrivateSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}
impl PrivateSecurity {
    pub(crate) fn new() -> Result<Self> {
        let sid = user_sid(unsafe { GetCurrentProcess() })?;
        let mut text = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut text) } == 0 {
            return Err(std::io::Error::last_os_error()).context("encoding user SID");
        }
        let mut length = 0;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        let sid_text = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })?;
        unsafe {
            LocalFree(text.cast());
        }
        let sddl = wide(OsStr::new(&format!(
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid_text})"
        )));
        let mut descriptor = std::ptr::null_mut();
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
    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
    fn apply(&self, file: &File) -> Result<()> {
        let (mut present, mut defaulted, mut dacl) = (0, 0, std::ptr::null_mut());
        if unsafe {
            GetSecurityDescriptorDacl(self.descriptor, &mut present, &mut dacl, &mut defaulted)
        } == 0
            || present == 0
        {
            bail!("private DACL is unavailable");
        }
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
impl Drop for PrivateSecurity {
    fn drop(&mut self) {
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
        .open(path)?;
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
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
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
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
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
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        assert_ne!(snapshot, INVALID_HANDLE_VALUE);
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut ids = Vec::new();
        let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                ids.push(entry.th32ThreadID);
            }
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
            unsafe { GetThreadId(fast.as_raw_handle()) },
            owned[0],
            "per-process lookup must name the suspended primary thread"
        );
        assert_eq!(unsafe { ResumeThread(fast.as_raw_handle()) }, 1);
        assert!(child.wait().expect("waiting for child").success());
    }
}
