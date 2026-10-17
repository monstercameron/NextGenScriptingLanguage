//! Owner-only native named pipes behind the existing `--socket PATH` interface.
//! The path is a local namespace key, not a TCP endpoint or filesystem socket.
use crate::platform::windows::{user_sid, wide, PrivateSecurity};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::Duration;
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Pipes::*, Threading::*, IO::*},
};

fn io_error(error: anyhow::Error) -> io::Error {
    io::Error::other(error.to_string())
}
fn event() -> io::Result<OwnedHandle> {
    // SAFETY: null security attributes and a null name give a default, unnamed event.
    // The manual-reset flag is 1 and the initial state is non-signalled.
    // Only null pointers are passed, so no buffer must stay alive.
    // A NULL return is checked below before the handle is adopted.
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is non-null (checked above) and was just returned by CreateEventW,
    // so nothing else owns it. The OwnedHandle becomes the sole owner.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}
fn name(path: &Path) -> io::Result<Vec<u16>> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    // SAFETY: GetCurrentProcess has no preconditions. It returns the pseudo-handle
    // for this process, which is always valid and needs no CloseHandle.
    let sid = user_sid(unsafe { GetCurrentProcess() }).map_err(io_error)?;
    let mut hash = Sha256::new();
    hash.update(sid);
    hash.update(path.as_os_str().as_encoded_bytes());
    Ok(wide(std::ffi::OsStr::new(&format!(
        "\\\\.\\pipe\\execute-{:x}",
        hash.finalize()
    ))))
}
pub(crate) struct Stream {
    handle: OwnedHandle,
    server: bool,
    read_timeout: AtomicU64,
    write_timeout: AtomicU64,
}
impl Stream {
    fn new(handle: OwnedHandle, server: bool) -> Self {
        Self {
            handle,
            server,
            read_timeout: AtomicU64::new(2000),
            write_timeout: AtomicU64::new(2000),
        }
    }
    pub(crate) fn connect(path: &Path) -> io::Result<Self> {
        let name = name(path)?;
        for attempt in 0..2 {
            // SAFETY: name is the NUL-terminated wide path from name(), owned by this frame
            // and alive for the call. Share mode 0 and null security attributes are valid.
            // FILE_FLAG_OVERLAPPED is required by transfer(). INVALID_HANDLE_VALUE is checked
            // (raw != INVALID_HANDLE_VALUE) before the handle is adopted.
            let raw = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if raw != INVALID_HANDLE_VALUE {
                return Ok(Self::new(
                    // SAFETY: raw passed the raw != INVALID_HANDLE_VALUE check above and is not
                    // stored anywhere else, so this OwnedHandle is its only owner.
                    unsafe { OwnedHandle::from_raw_handle(raw) },
                    false,
                ));
            }
            let error = io::Error::last_os_error();
            if attempt == 0 && error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
                // SAFETY: name is the NUL-terminated pipe path, still owned by this frame.
                // WaitNamedPipeW blocks for at most 2000 ms waiting for an instance. Its
                // return value is ignored because the loop retries CreateFileW once.
                unsafe {
                    WaitNamedPipeW(name.as_ptr(), 2000);
                }
            } else {
                return Err(error);
            }
        }
        unreachable!()
    }
    pub(crate) fn set_read_timeout(&self, value: Option<Duration>) -> io::Result<()> {
        Self::timeout(&self.read_timeout, value)
    }
    pub(crate) fn set_write_timeout(&self, value: Option<Duration>) -> io::Result<()> {
        Self::timeout(&self.write_timeout, value)
    }
    fn timeout(target: &AtomicU64, value: Option<Duration>) -> io::Result<()> {
        if value.is_some_and(|d| d.is_zero()) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        target.store(
            value.map_or(0, |d| d.as_millis().clamp(1, u64::MAX as u128) as u64),
            Ordering::Relaxed,
        );
        Ok(())
    }
    pub(crate) fn set_nonblocking(&self, enabled: bool) -> io::Result<()> {
        if enabled {
            Err(io::ErrorKind::Unsupported.into())
        } else {
            Ok(())
        }
    }
    pub(crate) fn peer_is_owner(&self) -> bool {
        let mut pid = 0;
        // SAFETY: self.handle is a named-pipe handle: a server instance from Pending::new
        // when self.server is set, otherwise a client handle from connect().
        // Each end gets the matching query. pid is a live local u32, read only when ok != 0.
        let ok = unsafe {
            if self.server {
                GetNamedPipeClientProcessId(self.handle.as_raw_handle(), &mut pid)
            } else {
                GetNamedPipeServerProcessId(self.handle.as_raw_handle(), &mut pid)
            }
        };
        if ok == 0 {
            return false;
        }
        // SAFETY: PROCESS_QUERY_LIMITED_INFORMATION is enough for OpenProcessToken with
        // TOKEN_QUERY in user_sid(). bInheritHandle is 0. pid is the peer reported by the
        // pipe query above. A NULL result, for example after the peer exited, is checked
        // below.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return false;
        }
        // SAFETY: raw is non-null (checked above) and was just returned by OpenProcess,
        // so this OwnedHandle is its only owner and closes it when process is dropped.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        let users = (
            user_sid(process.as_raw_handle()),
            // SAFETY: GetCurrentProcess returns the always-valid pseudo-handle for this process.
            user_sid(unsafe { GetCurrentProcess() }),
        );
        matches!(users,(Ok(peer),Ok(ours)) if peer==ours)
    }
    fn transfer(&self, buffer: *mut u8, length: usize, write: bool) -> io::Result<usize> {
        let event = event()?;
        // SAFETY: OVERLAPPED is plain C data (integers, pointers and a union), and an
        // all-zero bit pattern is valid. hEvent is set on the next line, before any use.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.as_raw_handle();
        let mut count = 0;
        // SAFETY: self.handle is a pipe opened with FILE_FLAG_OVERLAPPED. buffer addresses
        // a live allocation of at least length bytes; length is the caller's slice length
        // capped at u32::MAX. On writes the buffer is only read. count and overlapped are
        // live locals. A pending request is either waited on for the event, or cancelled
        // and waited on in GetOverlappedResult below, so the kernel never touches buffer
        // or overlapped after transfer() returns.
        let ready = unsafe {
            if write {
                WriteFile(
                    self.handle.as_raw_handle(),
                    buffer,
                    length.min(u32::MAX as usize) as u32,
                    &mut count,
                    &mut overlapped,
                )
            } else {
                ReadFile(
                    self.handle.as_raw_handle(),
                    buffer,
                    length.min(u32::MAX as usize) as u32,
                    &mut count,
                    &mut overlapped,
                )
            }
        };
        if ready == 0 {
            let error = io::Error::last_os_error();
            if !write && error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
            let timeout = if write {
                self.write_timeout.load(Ordering::Relaxed)
            } else {
                self.read_timeout.load(Ordering::Relaxed)
            };
            // SAFETY: event is the live manual-reset event named by overlapped.hEvent, so the
            // wait observes this request. The argument is INFINITE only when the stored
            // timeout is 0. Any other value is clamped to at most INFINITE - 1, so a finite
            // budget never becomes an infinite wait.
            let wait = unsafe {
                WaitForSingleObject(
                    event.as_raw_handle(),
                    if timeout == 0 {
                        INFINITE
                    } else {
                        timeout.min(u64::from(INFINITE - 1)) as u32
                    },
                )
            };
            if wait != WAIT_OBJECT_0 {
                // SAFETY: the wait did not signal, so the request is still pending. CancelIoEx
                // cancels only this request on this handle, and GetOverlappedResult with bWait
                // set blocks until the kernel has completed it. After this block the kernel no
                // longer references overlapped or buffer. Both return values are ignored; the
                // error returned comes from wait.
                // An OVERLAPPED and its buffer must outlive cancellation completion.
                unsafe {
                    CancelIoEx(self.handle.as_raw_handle(), &overlapped);
                    GetOverlappedResult(self.handle.as_raw_handle(), &overlapped, &mut count, 1);
                }
                return Err(if wait == WAIT_TIMEOUT {
                    io::ErrorKind::TimedOut.into()
                } else {
                    io::Error::last_os_error()
                });
            }
            // SAFETY: reached only when the wait returned WAIT_OBJECT_0 (the early return
            // above handles the others), so the event is signalled and the request has
            // completed. bWait is 0, so the call does not block. overlapped and count are
            // still live.
            if unsafe {
                GetOverlappedResult(self.handle.as_raw_handle(), &overlapped, &mut count, 0)
            } == 0
            {
                let error = io::Error::last_os_error();
                if !write && error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    return Ok(0);
                }
                return Err(error);
            }
        }
        Ok(count as usize)
    }
}
impl Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            Ok(0)
        } else {
            self.transfer(buffer.as_mut_ptr(), buffer.len(), false)
        }
    }
}
impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buffer)
    }
}
impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            Ok(0)
        } else {
            self.transfer(buffer.as_ptr().cast_mut(), buffer.len(), true)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Pending {
    handle: Option<OwnedHandle>,
    _event: OwnedHandle,
    overlapped: Box<OVERLAPPED>,
    waiting: bool,
}
// SAFETY: every field is Send except the raw HANDLE inside the boxed OVERLAPPED,
// which names the event owned by _event in the same struct. Kernel handles are
// not thread-affine. CancelIoEx and GetOverlappedResult, used in Drop and ready(),
// act on a request from any thread, unlike CancelIo.
// The kernel references the stable boxed OVERLAPPED and the owned event, never
// the surrounding Rust struct. Access is serialized by Listener's mutex.
unsafe impl Send for Pending {}
impl Pending {
    fn new(name: &[u16], first: bool) -> io::Result<Self> {
        let security = PrivateSecurity::new().map_err(io_error)?;
        let attributes = security.attributes();
        // SAFETY: name is the NUL-terminated wide pipe name from name(), alive for the call.
        // attributes borrows security (its lifetime keeps the descriptor alive through this
        // call), and CreateNamedPipeW reads the attributes only during the call. Buffer sizes and
        // nMaxInstances are advisory. FILE_FLAG_OVERLAPPED is set for the ConnectNamedPipe
        // below. INVALID_HANDLE_VALUE is checked before the handle is adopted.
        let raw = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX
                    | FILE_FLAG_OVERLAPPED
                    | if first {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        0
                    },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                attributes.as_ptr(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a pipe handle returned by CreateNamedPipeW, checked against
        // INVALID_HANDLE_VALUE above and held nowhere else; this OwnedHandle is the sole owner.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let event = event()?;
        // SAFETY: an all-zero OVERLAPPED is a valid value (see transfer()). Boxing it gives a
        // stable address that the kernel can keep referring to after Pending moves.
        let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        overlapped.hEvent = event.as_raw_handle();
        // SAFETY: handle is an overlapped pipe instance. overlapped is heap-allocated, and
        // its hEvent is the event owned by _event. Both are stored in the Pending returned
        // here, so they outlive any pending connect. Drop cancels and waits on a request
        // that is still marked waiting before any field is freed.
        let ready = unsafe { ConnectNamedPipe(handle.as_raw_handle(), &mut *overlapped) };
        let waiting = if ready == 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(n) if n == ERROR_IO_PENDING as i32 => true,
                Some(n) if n == ERROR_PIPE_CONNECTED as i32 => false,
                _ => return Err(e),
            }
        } else {
            false
        };
        Ok(Self {
            handle: Some(handle),
            _event: event,
            overlapped,
            waiting,
        })
    }
    fn ready(&mut self) -> io::Result<bool> {
        if !self.waiting {
            return Ok(true);
        }
        let mut count = 0;
        // SAFETY: waiting is set only after ConnectNamedPipe returned pending on this handle
        // with this boxed OVERLAPPED. self.handle is Some here: take() runs only in accept(),
        // on a value that is discarded afterwards. bWait is 0, so the call does not block.
        // count is a live local.
        if unsafe {
            GetOverlappedResult(
                self.handle.as_ref().unwrap().as_raw_handle(),
                &*self.overlapped,
                &mut count,
                0,
            )
        } != 0
        {
            self.waiting = false;
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32) {
            Ok(false)
        } else {
            Err(e)
        }
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.waiting {
            if let Some(handle) = &self.handle {
                let mut count = 0;
                // SAFETY: this runs only while waiting is set, so the connect is still pending
                // on handle with &*self.overlapped. CancelIoEx cancels it, and GetOverlappedResult
                // with bWait set blocks until the kernel has finished with overlapped and
                // the event.
                // Drop runs before the fields are freed, so both are still allocated.
                unsafe {
                    CancelIoEx(handle.as_raw_handle(), &*self.overlapped);
                    GetOverlappedResult(handle.as_raw_handle(), &*self.overlapped, &mut count, 1);
                }
            }
        }
    }
}
pub(crate) struct Listener {
    name: Vec<u16>,
    pending: Mutex<Pending>,
}
impl Listener {
    pub(crate) fn bind(path: &Path) -> io::Result<Self> {
        let name = name(path)?;
        let pending = Pending::new(&name, true)?;
        Ok(Self {
            name,
            pending: Mutex::new(pending),
        })
    }
    pub(crate) fn set_nonblocking(&self, _value: bool) -> io::Result<()> {
        Ok(())
    }
    pub(crate) fn accept(&self) -> io::Result<(Stream, ())> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| io::Error::other("pipe lock poisoned"))?;
        if !pending.ready()? {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let next = Pending::new(&self.name, false)?;
        let mut connected = std::mem::replace(&mut *pending, next);
        Ok((Stream::new(connected.handle.take().unwrap(), true), ()))
    }
}
