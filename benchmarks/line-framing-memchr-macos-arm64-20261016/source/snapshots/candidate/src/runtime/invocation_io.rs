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
