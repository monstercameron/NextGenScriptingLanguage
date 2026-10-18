//! Per-applet stream choices for native execution.
//!
//! Captured streams are piped to the runtime. Discarded streams are attached to
//! the operating system's null device, so discarded bytes never enter a Rust
//! buffer or a bounded capture pipe. Inherited streams attach to the parent
//! process descriptors. Stdout between pipeline stages remains a direct native
//! byte pipe and is not affected by the terminal-stage disposition.

pub use crate::contracts::{StdioContract, StdioDisposition};

/// Fully resolved stream choices after applying legacy runtime defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedStdio {
    pub stdout: StdioDisposition,
    pub stderr: StdioDisposition,
}

impl ResolvedStdio {
    /// Apply optional manifest overrides while preserving the caller's legacy
    /// defaults for streams the manifest leaves unspecified.
    pub fn resolve(
        contract: Option<&StdioContract>,
        stdout_default: StdioDisposition,
        stderr_default: StdioDisposition,
    ) -> Self {
        let Some(contract) = contract else {
            return Self {
                stdout: stdout_default,
                stderr: stderr_default,
            };
        };
        Self {
            stdout: contract.stdout.unwrap_or(stdout_default),
            stderr: contract.stderr.unwrap_or(stderr_default),
        }
    }

    /// Apply a source-level override after manifest defaults. Callers must
    /// validate that the override is supported for this invocation before
    /// launch; discarded bytes use the same null-device path as manifest
    /// `stdio.stderr = "discard"`.
    pub fn discard_stderr(mut self, discard: bool) -> Self {
        if discard {
            self.stderr = StdioDisposition::Discard;
        }
        self
    }

    /// Intermediate stdout is always the next child's stdin; only the final
    /// process in a pipeline uses its configured stdout disposition.
    pub fn stdout_for_stage(self, terminal_stage: bool) -> StdioDisposition {
        if terminal_stage {
            self.stdout
        } else {
            StdioDisposition::Capture
        }
    }

    pub fn captures_terminal_stdout(self) -> bool {
        self.stdout == StdioDisposition::Capture
    }

    pub fn captures_stderr(self) -> bool {
        self.stderr == StdioDisposition::Capture
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn process_stdio(disposition: StdioDisposition) -> std::process::Stdio {
    match disposition {
        StdioDisposition::Capture => std::process::Stdio::piped(),
        StdioDisposition::Discard => std::process::Stdio::null(),
        StdioDisposition::Inherit => std::process::Stdio::inherit(),
    }
}

/// Read at most one bounded chunk into the final capture allocation. Callers retain
/// responsibility for readiness, cancellation and charging their output budgets.
#[cfg(unix)]
pub(super) fn read_into_capture(
    reader: &impl std::os::fd::AsFd,
    output: &mut Vec<u8>,
    maximum: usize,
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: read initializes exactly its nonnegative return count, never more
    // than the supplied buffer length. The borrowed descriptor stays open.
    unsafe {
        capture_read_into(output, maximum, |pointer, length| {
            let count = libc::read(reader.as_fd().as_raw_fd(), pointer.cast(), length);
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        })
    }
}

// The Windows prototype stays test-only: local measurements favor its existing
// chunked reader. Linux uses the descriptor path above.
#[cfg(all(test, windows))]
pub(super) fn read_into_capture(
    reader: &impl std::os::windows::io::AsHandle,
    output: &mut Vec<u8>,
    maximum: usize,
) -> std::io::Result<usize> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    // SAFETY: ReadFile initializes the reported count within the supplied buffer.
    // All callers supply synchronous handles, kept open by the borrow. The count
    // and null OVERLAPPED arguments live throughout this synchronous call.
    unsafe {
        capture_read_into(output, maximum.min(u32::MAX as usize), |pointer, length| {
            let mut count = 0;
            if ReadFile(
                reader.as_handle().as_raw_handle().cast(),
                pointer,
                length as u32,
                &mut count,
                std::ptr::null_mut(),
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    return Ok(0);
                }
                return Err(error);
            }
            Ok(count as usize)
        })
    }
}

/// # Safety
/// `read` must initialize its returned count of bytes at `pointer`, return at
/// most `length`, and neither retain the pointer nor access beyond `length`.
#[cfg(any(unix, all(test, windows)))]
unsafe fn capture_read_into(
    output: &mut Vec<u8>,
    maximum: usize,
    read: impl FnOnce(*mut u8, usize) -> std::io::Result<usize>,
) -> std::io::Result<usize> {
    let length = output.len();
    output.reserve(maximum);
    let pointer = output.spare_capacity_mut().as_mut_ptr().cast::<u8>();
    let count = read(pointer, maximum)?;
    // SAFETY: the callback contract guarantees that exactly the returned count
    // is initialized, and count <= maximum <= the reserved spare capacity. On
    // an error the old length is untouched, so uninitialized bytes stay hidden.
    unsafe { output.set_len(length + count) };
    Ok(count)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod lane_v2_capture_tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn lane_v2_capture_reader_writes_final_storage() {
        let mut output = Vec::with_capacity(64);
        output.extend_from_slice(b"prefix");
        let destination = output.spare_capacity_mut().as_mut_ptr().cast::<u8>();
        // SAFETY: the callback initializes and reports precisely three bytes.
        unsafe {
            capture_read_into(&mut output, 16, |pointer, maximum| {
                assert_eq!(maximum, 16);
                assert_eq!(
                    pointer, destination,
                    "capture must read into the final allocation"
                );
                pointer.copy_from_nonoverlapping(b"end".as_ptr(), 3);
                Ok(3)
            })
        }
        .unwrap();
        assert_eq!(output, b"prefixend");
    }

    #[test]
    fn lane_v2_capture_short_reads_eof_and_error_preserve_length() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abcdef").unwrap();
        file.rewind().unwrap();
        let mut output = b"prefix".to_vec();
        assert_eq!(read_into_capture(&file, &mut output, 2).unwrap(), 2);
        assert_eq!(read_into_capture(&file, &mut output, 16).unwrap(), 4);
        assert_eq!(read_into_capture(&file, &mut output, 16).unwrap(), 0);
        assert_eq!(output, b"prefixabcdef");
        let original = output.clone();
        // SAFETY: this callback does not initialize or report any bytes.
        let error = unsafe {
            capture_read_into(&mut output, 16, |_, _| {
                Err(std::io::ErrorKind::WouldBlock.into())
            })
        }
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(output, original);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod lane_v2_capture_benchmark {
    use super::*;
    use std::io::{Read, Seek, Write};
    use std::time::Instant;

    #[test]
    #[ignore = "100 MiB pipe capture benchmark; run in isolation"]
    fn lane_v2_pipe_capture_100mb() {
        let mut timings = [Vec::new(), Vec::new()];
        for batch in 0..7 {
            for mode in if batch % 2 == 0 { [0, 1] } else { [1, 0] } {
                let (mut reader, mut writer) = std::io::pipe().unwrap();
                let producer = std::thread::spawn(move || {
                    let chunk = [b'x'; 32768];
                    for _ in 0..3200 {
                        writer.write_all(&chunk).unwrap();
                    }
                });
                let start = Instant::now();
                let mut output = Vec::with_capacity(100 * 1024 * 1024);
                if mode == 0 {
                    let mut chunk = [0; 32768];
                    loop {
                        let count = reader.read(&mut chunk).unwrap();
                        if count == 0 {
                            break;
                        }
                        output.extend_from_slice(&chunk[..count]);
                    }
                } else {
                    let mut chunk = [0; 32768];
                    loop {
                        let count = if output.capacity() - output.len() >= chunk.len() {
                            read_into_capture(&reader, &mut output, chunk.len()).unwrap()
                        } else {
                            let count = reader.read(&mut chunk).unwrap();
                            output.extend_from_slice(&chunk[..count]);
                            count
                        };
                        if count == 0 {
                            break;
                        }
                    }
                }
                timings[mode].push(start.elapsed());
                producer.join().unwrap();
                assert_eq!(output.len(), 100 * 1024 * 1024);
                assert!(output.iter().all(|byte| *byte == b'x'));
                std::hint::black_box(output);
            }
        }
        for timing in &mut timings {
            timing.sort_unstable();
        }
        eprintln!(
            "100 MiB pipe capture: chunk/copy {:?}; direct {:?}",
            timings[0][3], timings[1][3]
        );
    }
    #[test]
    #[ignore = "100 MiB capture copy benchmark; run in isolation"]
    fn lane_v2_capture_100mb() {
        let mut file = tempfile::tempfile().unwrap();
        let chunk = [b'x'; 32768];
        for _ in 0..3200 {
            file.write_all(&chunk).unwrap();
        }
        let mut legacy = Vec::new();
        let mut direct = Vec::new();
        for batch in 0..7 {
            for mode in if batch % 2 == 0 { [0, 1] } else { [1, 0] } {
                file.rewind().unwrap();
                let start = Instant::now();
                let mut output = Vec::with_capacity(100 * 1024 * 1024);
                if mode == 0 {
                    let mut chunk = [0; 32768];
                    loop {
                        let count = file.read(&mut chunk).unwrap();
                        if count == 0 {
                            break;
                        }
                        output.extend_from_slice(&chunk[..count]);
                    }
                } else {
                    let mut chunk = [0; 32768];
                    loop {
                        let count = if output.capacity() - output.len() >= chunk.len() {
                            read_into_capture(&file, &mut output, chunk.len()).unwrap()
                        } else {
                            let count = file.read(&mut chunk).unwrap();
                            output.extend_from_slice(&chunk[..count]);
                            count
                        };
                        if count == 0 {
                            break;
                        }
                    }
                }
                let elapsed = start.elapsed();
                assert_eq!(output.len(), 100 * 1024 * 1024);
                assert!(output.iter().all(|byte| *byte == b'x'));
                std::hint::black_box(&output);
                if mode == 0 {
                    legacy.push(elapsed);
                } else {
                    direct.push(elapsed);
                }
            }
        }
        legacy.sort_unstable();
        direct.sort_unstable();
        eprintln!(
            "100 MiB capture: chunk/copy {:?}; direct {:?}",
            legacy[3], direct[3]
        );
    }
}
