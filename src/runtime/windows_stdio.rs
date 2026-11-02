//! Cancellable native Windows handle I/O, including redirected binary stdio.
use super::{check_deadline, Signals};
use anyhow::{bail, Context, Result};
use std::os::windows::io::AsRawHandle;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// Windows currently uses the synchronous native handle writer below. Keep the run-scoped
/// abstraction available to shared runtime code without changing Windows I/O behavior.
pub(super) struct OutputWriterPool;

impl OutputWriterPool {
    pub(super) fn new() -> Self {
        Self
    }
}

pub(super) fn write_with_deadline_owned_reusing(
    _pool: &OutputWriterPool,
    fd: i32,
    bytes: Vec<u8>,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    write_with_deadline(fd, &bytes, cancelled, deadline)
}
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Console::*, IO::CancelSynchronousIo},
};

fn handle(fd: i32) -> Result<HANDLE> {
    let kind = match fd {
        0 => STD_INPUT_HANDLE,
        1 => STD_OUTPUT_HANDLE,
        2 => STD_ERROR_HANDLE,
        _ => bail!("invalid standard handle"),
    };
    // SAFETY: kind is one of the three standard-handle constants chosen by the match above.
    // GetStdHandle takes no pointers. The handle it returns is not owned here, so it is never
    // closed; a null or INVALID_HANDLE_VALUE result is rejected on the next line.
    let handle = unsafe { GetStdHandle(kind) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        bail!("standard handle is unavailable");
    }
    Ok(handle)
}
fn cooperative<T: Send + 'static>(
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
    operation: impl FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
) -> Result<T> {
    check_deadline(deadline)?;
    if cancelled.load(Ordering::Relaxed) {
        bail!("workflow cancelled by signal");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = operation(worker_stop);
        let _ = sender.send(());
        result
    });
    loop {
        if cancelled.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
            stop.store(true, Ordering::Relaxed);
            // SAFETY: worker is a JoinHandle that is only joined after this loop, so the thread
            // handle it holds stays open here. CancelSynchronousIo affects only synchronous I/O
            // issued by that thread. If the thread already finished, the call fails harmlessly.
            // Repeated cancellation closes the narrow race between the owned
            // worker's stop check and its next synchronous kernel operation.
            unsafe {
                CancelSynchronousIo(worker.as_raw_handle());
            }
        }
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("standard I/O worker panicked"))?;
    check_deadline(deadline)?;
    if cancelled.load(Ordering::Relaxed) {
        bail!("workflow cancelled by signal");
    }
    result
}
pub fn write_cancellable(fd: i32, bytes: &[u8], cancelled: &AtomicBool) -> Result<()> {
    write_with_deadline(fd, bytes, cancelled, None)
}
pub fn write_with_deadline(
    fd: i32,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    let target = handle(fd)? as usize;
    let data = bytes.to_vec();
    cooperative(cancelled, deadline, move |stop| {
        write_all(target as HANDLE, &data, &stop)
    })
}

/// Maps arbitrary output bytes onto console text: valid UTF-8 passes through and
/// each invalid sequence becomes U+FFFD, so display never fails the run. Handles
/// that are not consoles keep the exact bytes in the branch below.
fn console_wide(data: &[u8]) -> Vec<u16> {
    String::from_utf8_lossy(data).encode_utf16().collect()
}

/// Whether this process's stdout is a Windows console. A native child that
/// inherits a console handle writes through its own runtime, which may reject
/// non-UTF-8 bytes before this process can replace them (Rust's stdio does);
/// callers must capture such a child's stdout instead and relay it through
/// `write_with_deadline`, whose console branch is lossy.
pub(super) fn stdout_is_console() -> bool {
    let Ok(target) = handle(1) else {
        return false;
    };
    let mut mode = 0;
    // SAFETY: target is the live standard-output handle from handle(), and
    // mode is a live local u32 for the out-parameter. A zero return means the
    // handle is redirected (pipe or file), which keeps exact bytes.
    unsafe { GetConsoleMode(target, &mut mode) != 0 }
}

/// Bytes from `bytes` (appended to `pending`) that are ready for a lossy
/// console write. A trailing UTF-8 sequence that is merely incomplete is held
/// in `pending` for the next chunk, so a character split across pipe reads is
/// not replaced; a complete invalid sequence is released at once, because the
/// console writer replaces it with U+FFFD.
pub(super) fn console_relay_ready(pending: &mut Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    pending.extend_from_slice(bytes);
    match std::str::from_utf8(pending) {
        Ok(_) => std::mem::take(pending),
        Err(error) if error.error_len().is_none() => {
            let ready = pending[..error.valid_up_to()].to_vec();
            pending.drain(..error.valid_up_to());
            ready
        }
        Err(_) => std::mem::take(pending),
    }
}

/// Writes every byte to `target`, checking `stop` between chunks and retrying partial
/// writes until all bytes are out.
fn write_all(target: HANDLE, data: &[u8], stop: &AtomicBool) -> Result<()> {
    let mut mode = 0;
    // SAFETY: target is a standard handle from handle(), and mode is a live local u32 for
    // the out-parameter. GetConsoleMode returns 0 for a non-console handle, which selects the
    // byte-write branch below.
    if unsafe { GetConsoleMode(target, &mut mode) } != 0 {
        let wide = console_wide(data);
        let mut offset = 0;
        while offset < wide.len() {
            if stop.load(Ordering::Relaxed) {
                bail!("output cancelled");
            }
            let count = (wide.len() - offset).min(16384);
            let mut written = 0;
            // SAFETY: target is a console handle, since GetConsoleMode succeeded above. The buffer
            // is wide[offset..] with count at most wide.len() - offset, so count UTF-16 units are
            // in bounds. written is a live local out-parameter, and the reserved argument is null.
            if unsafe {
                WriteConsoleW(
                    target,
                    wide[offset..].as_ptr().cast(),
                    count as u32,
                    &mut written,
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error()).context("writing terminal");
            }
            if written == 0 {
                bail!("writing terminal made no progress");
            }
            offset += written as usize;
        }
    } else {
        let mut offset = 0;
        while offset < data.len() {
            if stop.load(Ordering::Relaxed) {
                bail!("output cancelled");
            }
            let count = (data.len() - offset).min(32768);
            let mut written = 0;
            // SAFETY: count is at most data.len() - offset, so data[offset..] is in bounds and
            // the buffer is valid for the count bytes WriteFile may read. written is a live local
            // out-parameter. lpOverlapped is null, so target must be a synchronous handle. The
            // code assumes inherited handles are synchronous and cannot check that here.
            if unsafe {
                WriteFile(
                    target,
                    data[offset..].as_ptr(),
                    count as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error()).context("writing redirected output");
            }
            if written == 0 {
                bail!("writing output made no progress");
            }
            offset += written as usize;
        }
    }
    Ok(())
}

/// Final diagnostics, written as the process exits. This is a plain blocking write to
/// stderr with no cancel or deadline, so a slow reader cannot cause the message to be
/// dropped. The exit status is already non-zero, so a failed write has nothing else to
/// report.
pub fn write_diagnostic(bytes: &[u8]) {
    let Ok(target) = handle(2) else {
        return;
    };
    let _ = write_all(target, bytes, &AtomicBool::new(false));
}
pub(super) fn capture_stdin(
    limit: usize,
    signals: &Signals,
    timeout: Option<Duration>,
) -> Result<Vec<u8>> {
    let deadline = timeout
        .map(|d| {
            Instant::now()
                .checked_add(d)
                .context("stdin timeout too large")
        })
        .transpose()?;
    cooperative(&signals.cancelled, deadline, move |stop| {
        let mut output = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if stop.load(Ordering::Relaxed) {
                bail!("stdin cancelled");
            }
            use std::io::Read;
            let read = match std::io::stdin().lock().read(&mut chunk) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(output),
                Err(error) => return Err(error).context("reading stdin"),
            };
            if read == 0 {
                return Ok(output);
            }
            if read as usize > limit.saturating_sub(output.len()) {
                bail!("stdin exceeds maximum of {limit} bytes");
            }
            output.extend_from_slice(&chunk[..read as usize]);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::console_wide;

    fn displayed(data: &[u8]) -> String {
        String::from_utf16_lossy(&console_wide(data))
    }

    #[test]
    fn invalid_console_byte_becomes_replacement_char_and_keeps_exit_zero_path() {
        assert_eq!(displayed(&[0xFF, 0x41]), "�A");
    }

    #[test]
    fn valid_utf8_console_output_passes_through_unchanged() {
        assert_eq!(displayed("héllo 雪".as_bytes()), "héllo 雪");
    }

    #[test]
    fn empty_console_output_stays_empty() {
        assert!(console_wide(&[]).is_empty());
    }

    #[test]
    fn truncated_multibyte_sequence_becomes_one_replacement_char() {
        assert_eq!(displayed(&[0xE2, 0x82]), "�");
    }

    #[test]
    fn relay_releases_complete_invalid_sequence_for_replacement() {
        let mut pending = Vec::new();
        let ready = super::console_relay_ready(&mut pending, &[0xFF, 0x41]);
        assert!(pending.is_empty());
        assert_eq!(displayed(&ready), "�A");
    }

    #[test]
    fn relay_holds_a_multibyte_character_split_across_chunks() {
        let mut pending = Vec::new();
        assert!(super::console_relay_ready(&mut pending, &[0xE2, 0x82]).is_empty());
        let ready = super::console_relay_ready(&mut pending, &[0xAC, 0x41]);
        assert!(pending.is_empty());
        assert_eq!(displayed(&ready), "€A");
    }
}
