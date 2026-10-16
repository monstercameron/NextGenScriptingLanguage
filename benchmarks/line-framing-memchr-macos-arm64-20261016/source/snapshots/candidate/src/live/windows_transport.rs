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
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}
fn name(path: &Path) -> io::Result<Vec<u16>> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let sid = user_sid(unsafe { GetCurrentProcess() }).map_err(io_error)?;
    let mut hash = Sha256::new();
    hash.update(sid);
    hash.update(path.as_os_str().as_encoded_bytes());
    Ok(wide(std::ffi::OsStr::new(&format!(
        "\\\\.\\pipe\\execute-{:x}",
        hash.finalize()
    ))))
}
pub(super) struct Stream {
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
    pub(super) fn connect(path: &Path) -> io::Result<Self> {
        let name = name(path)?;
        for attempt in 0..2 {
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
                    unsafe { OwnedHandle::from_raw_handle(raw) },
                    false,
                ));
            }
            let error = io::Error::last_os_error();
            if attempt == 0 && error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
                unsafe {
                    WaitNamedPipeW(name.as_ptr(), 2000);
                }
            } else {
                return Err(error);
            }
        }
        unreachable!()
    }
    pub(super) fn set_read_timeout(&self, value: Option<Duration>) -> io::Result<()> {
        Self::timeout(&self.read_timeout, value)
    }
    pub(super) fn set_write_timeout(&self, value: Option<Duration>) -> io::Result<()> {
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
    pub(super) fn set_nonblocking(&self, enabled: bool) -> io::Result<()> {
        if enabled {
            Err(io::ErrorKind::Unsupported.into())
        } else {
            Ok(())
        }
    }
    pub(super) fn peer_is_owner(&self) -> bool {
        let mut pid = 0;
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
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return false;
        }
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        let users = (
            user_sid(process.as_raw_handle()),
            user_sid(unsafe { GetCurrentProcess() }),
        );
        matches!(users,(Ok(peer),Ok(ours)) if peer==ours)
    }
    fn transfer(&self, buffer: *mut u8, length: usize, write: bool) -> io::Result<usize> {
        let event = event()?;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.as_raw_handle();
        let mut count = 0;
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
// The kernel references the stable boxed OVERLAPPED and the owned event, never
// the surrounding Rust struct. Access is serialized by Listener's mutex.
unsafe impl Send for Pending {}
impl Pending {
    fn new(name: &[u16], first: bool) -> io::Result<Self> {
        let security = PrivateSecurity::new().map_err(io_error)?;
        let attributes = security.attributes();
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
                &attributes,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let event = event()?;
        let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        overlapped.hEvent = event.as_raw_handle();
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
                unsafe {
                    CancelIoEx(handle.as_raw_handle(), &*self.overlapped);
                    GetOverlappedResult(handle.as_raw_handle(), &*self.overlapped, &mut count, 1);
                }
            }
        }
    }
}
pub(super) struct Listener {
    name: Vec<u16>,
    pending: Mutex<Pending>,
}
impl Listener {
    pub(super) fn bind(path: &Path) -> io::Result<Self> {
        let name = name(path)?;
        let pending = Pending::new(&name, true)?;
        Ok(Self {
            name,
            pending: Mutex::new(pending),
        })
    }
    pub(super) fn set_nonblocking(&self, _value: bool) -> io::Result<()> {
        Ok(())
    }
    pub(super) fn accept(&self) -> io::Result<(Stream, ())> {
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
