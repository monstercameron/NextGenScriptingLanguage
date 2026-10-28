//! GitHub Actions workflow-command hardening for finite runs.
//!
//! The Actions runner interprets `::` workflow commands (`::error::`,
//! `::add-mask::`, `::stop-commands::` and friends) after trimming leading
//! whitespace, and legacy `##[` commands (for example `##[error]`) anywhere in
//! a line. A program that processes untrusted content can therefore forge log
//! commands through the child output Execute forwards, and secrets Execute
//! already knows would otherwise appear unmasked in the surrounding log.
//!
//! When hardening is enabled, Execute neutralizes those prefixes in the bytes
//! it forwards to the terminal (streamed step output, re-emitted captured
//! stderr, the final program output and final diagnostics) by inserting a
//! zero-width space into the prefix: between the colons of a line-leading
//! `::` (leading whitespace included) and between the hashes of any `##[`
//! sequence. The bytes stay human-readable but no longer parse as a workflow
//! command. Every newly registered secret value is additionally masked with
//! `::add-mask::` so the runner redacts it from the log from that point on.
//! Each mask unconditionally opens its own log line with a leading line break
//! (plus its trailing newline) because the runner only recognizes `::`
//! commands at a line start; the shared sanitizer state cannot prove the
//! shared sink is at a line start under concurrent writes, so a conditional
//! break could still glue the mask mid-line. The pre-registered-secret sweep
//! at run start runs before the terminal state reset for the same reason.
//!
//! Hardening is opt-in through [`set_enabled`]. The `execute` CLI enables it for
//! `run` and `resume` when `GITHUB_ACTIONS` is `"true"`. Library hosts running
//! under Actions should call [`set_enabled`] before executing; it defaults to
//! off so library tests and framed protocols (LSP, DAP, repl JSON) never see
//! mask lines or rewritten bytes unless the host asked for them. A host that
//! registered secrets before enabling must call [`emit_masks_for_registry`]
//! (each finite run also sweeps its own registry at start); secrets registered
//! afterwards mask themselves through the registry hook. Values Execute has
//! not yet learned — for example dynamic command output before the producing
//! step completes — cannot be masked before discovery; sensitive-marked steps
//! are therefore captured and only copied to the log after registration, so
//! the mask precedes the value. Already-echoed bytes are never retroactively
//! redacted by the runner.
//!
//! Under hardening, terminal-bound child streams are captured and re-emitted
//! through the sanitizing writers instead of inheriting the raw descriptors,
//! including manifest `inherit` dispositions and the streamed-step inherit
//! fast path. Program-level redirects to files stay raw in their files; only
//! the bytes Execute itself copies to the log are filtered. `hand over to`
//! is refused before any step runs: the handed-over program replaces the
//! process and its terminal writes after `exec` cannot be intercepted.
//! Detached launches, observers and record/replay captures intentionally see
//! pre-filter bytes (they own their own sinks, not the job log) and stay raw;
//! `execute check --format github` intentionally emits workflow commands and
//! is unaffected.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Process-wide hardening switch. Set once before execution; read on every
/// forwarded write. `SeqCst` keeps the reasoning simple across worker threads.
static HARDENING: AtomicBool = AtomicBool::new(false);

/// Enable or disable Actions hardening for this process. The CLI calls this
/// for `run`/`resume` based on `GITHUB_ACTIONS`; library tests leave it off.
pub fn set_enabled(on: bool) {
    HARDENING.store(on, Ordering::SeqCst);
}

/// Whether forwarded child output is neutralized and secrets are masked.
pub fn enabled() -> bool {
    HARDENING.load(Ordering::SeqCst)
}

/// Whether the current process runs inside GitHub Actions.
pub fn is_ci() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some_and(|value| value == "true")
}

/// Enable hardening when running under GitHub Actions. Returns the new state.
/// The CLI calls this at the start of `run`/`resume`; library hosts running
/// under Actions should call it before executing. Secrets registered before
/// this call are not covered here — there is no global registry list to
/// sweep — so hosts with pre-registered secrets must call
/// [`emit_masks_for_registry`]; every finite run sweeps its own registry at
/// start as a backstop.
pub fn enable_if_ci() -> bool {
    let on = is_ci();
    set_enabled(on);
    on
}

/// Enable hardening without ever disabling it. Long-lived hosts with
/// concurrent sessions (for example live `run --control` next to an
/// interactive `serve`) use this so one session cannot flip the
/// process-global switch off while another session is streaming.
pub fn enable_if_ci_no_disable() -> bool {
    let on = is_ci();
    if on {
        set_enabled(true);
    }
    on
}

/// Process-wide count of outermost finite runs currently setting up their
/// terminal boundary. Thread-local depth alone cannot see a concurrent
/// outermost run on another thread; without this two such runs would both
/// sweep and reset, and the reset would drop held split-prefix bytes the
/// other run is still owed.
static ACTIVE_OUTERMOST_RUNS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Serializes the sweep-before-reset boundary so two runs starting together
/// cannot interleave one run's sweep with the other's reset.
static RUN_BOUNDARY_LOCK: Mutex<()> = Mutex::new(());

/// Serializes filtering with the actual write to the shared merged sink.
///
/// Every hardened terminal write (filtered `fd` 1/2 bytes, mask lines, final
/// output) holds this lock from its sanitize step until its bytes have
/// reached the sink. That closes two races the split filter-then-write path
/// left open: a partial landing mid-`write_all` and splitting a mask line
/// (lost mask, secret unmasked), and a chunk filtered as mid-line but
/// written after a mask resync so a raw `::error::` becomes line-leading in
/// the merged job log (forged workflow command). Stdout and stderr share one
/// lock because the runner merges both descriptors into one log: per-fd
/// kernel ordering cannot order that merged view, so only in-process
/// filter+write atomicity across both streams can.
///
/// Lock order is always `TERMINAL_SINK_LOCK`, then the merged-terminal
/// claim, then (for pooled writes) the output-target reservation, then the
/// stdout sanitizer, then the stderr sanitizer. No path acquires them in
/// reverse, so the nesting cannot deadlock. Claiming before filtering matters
/// for the cancel drain below: even if the drain expires while a worker is
/// still kernel-blocked, the next hardened write — either descriptor, pooled
/// or direct — blocks on the merged claim before it filters, so it cannot
/// classify before the prior bytes land. At most one hardened terminal write
/// is in flight process-wide, so filter order always matches landing order
/// and the shared line state always reflects every preceding byte, whichever
/// descriptor carried it. The lock is held across the blocking terminal write
/// on purpose: releasing it trades a stalled terminal wedging hardened
/// output for silent forgery/mask-loss, and the fail-closed choice is to
/// stall together (the descriptors share one sink, so stalling together
/// preserves destination semantics). File redirects and non-hardened writes
/// never take this lock and stay concurrent. Cancelled or expired pooled
/// writes signal `stop` and then drain the worker with a bound instead of
/// releasing early or hanging: steady worker progress (a slow reader)
/// extends the drain up to the caller deadline so order stays intact, while
/// no progress expires it — at the caller deadline, or after a short grace
/// without one — and the cancellation is reported. The in-flight worker keeps
/// both the sink-era order and the merged claim until it finishes, so even a
/// forced drain expiry leaves every later hardened write ordered behind it;
/// waiters blocked on the claim keep checking their own cancel/deadline, so
/// cancellation stays bounded while uncancelled waiters stall together rather
/// than forge. In `debug` builds the filter/flush primitives assert this lock
/// is held, so a future caller that filters without the sink fails tests
/// instead of silently re-opening the split (the check is lock-state, not
/// ownership: it cannot see which thread holds the lock).
///
/// Concurrent hardened runs against one terminal are chunk-serialized by
/// this lock (each chunk filters and lands atomically, so no raw command can
/// execute), but their lines still interleave. Hosts that need exact
/// per-run filtering must serialize those runs; the boundary in
/// [`enter_finite_run`] only covers setup.
static TERMINAL_SINK_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the merged-sink serialization lock. Production terminal writes
/// hold the returned guard across their filter step and their sink write;
/// see `TERMINAL_SINK_LOCK` for the order and the fail-closed rules.
pub fn terminal_sink_lock() -> std::sync::MutexGuard<'static, ()> {
    TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Which hardened terminal write is currently in flight, if any. Pooled
/// writers move their claim into the worker (held until the kernel write
/// finishes); direct writers (masks, finals, the serve notice) hold theirs
/// across the blocking `write_all`. Either way at most one hardened write is
/// in flight, so no later filter can classify before prior bytes land —
/// including after a forced cancel-drain expiry that released the sink lock
/// early. The value is an owned generation token, not a plain boolean, so a
/// stale guard (or a reset racing a finished worker) can never release
/// another writer's claim: `Drop` clears the slot only when it still holds
/// this guard's own token.
static MERGED_TERMINAL_ACTIVE: Mutex<Option<u64>> = Mutex::new(None);
static MERGED_TERMINAL_NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Proof that this thread (or its worker) is the one hardened terminal write
/// currently in flight. Dropping releases the claim for the next writer, but
/// only when this guard still owns it: a stale drop after a newer writer
/// claimed the slot is a no-op instead of a false unlock.
/// The guard holds no lock — just the token above — so it is `Send` and can
/// move into a pool worker; the brief internal mutex is only taken
/// momentarily inside `try_claim_merged_terminal` and `Drop`, never across
/// other locks. Lock order stays `TERMINAL_SINK_LOCK`, then this claim.
pub struct MergedTerminalReservation(u64);

impl Drop for MergedTerminalReservation {
    fn drop(&mut self) {
        let mut active = MERGED_TERMINAL_ACTIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *active == Some(self.0) {
            *active = None;
        }
    }
}

/// Take the merged-terminal claim without waiting, or `None` while another
/// hardened write is in flight. Cancellable paths poll this with their own
/// cancel/deadline via [`reserve_merged_terminal`]; paths with no cancel or
/// deadline to report use [`reserve_merged_terminal_blocking`].
pub(crate) fn try_claim_merged_terminal() -> Option<MergedTerminalReservation> {
    let mut active = MERGED_TERMINAL_ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if active.is_some() {
        return None;
    }
    let id = MERGED_TERMINAL_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    *active = Some(id);
    Some(MergedTerminalReservation(id))
}

fn check_merged_cancel(cancelled: &AtomicBool, deadline: Option<Instant>) -> anyhow::Result<()> {
    if let Some(end) = deadline {
        if Instant::now() >= end {
            anyhow::bail!("terminal wait reached its deadline");
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        anyhow::bail!("program cancelled by signal");
    }
    Ok(())
}

/// Take the merged-terminal claim, waiting until the in-flight hardened
/// write finishes. The wait keeps checking the caller's own cancel flag and
/// deadline, so cancellation stays bounded: a cancelled waiter reports
/// cancellation without writing anything, while an uncancelled waiter
/// without a deadline stalls together with the wedged terminal rather than
/// forging (fail-closed). The claim moves into the writer (or is held
/// across the direct `write_all`), so every later hardened write — either
/// descriptor, pooled or direct — stays ordered behind this one even after
/// a forced cancel-drain expiry.
pub fn reserve_merged_terminal(
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> anyhow::Result<MergedTerminalReservation> {
    loop {
        check_merged_cancel(cancelled, deadline)?;
        if let Some(reservation) = try_claim_merged_terminal() {
            return Ok(reservation);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Take the merged-terminal claim, waiting until the in-flight hardened
/// write finishes. Only for paths with no cancel or deadline to report
/// (process-exit diagnostics, best-effort registry-hook masks, tests): they
/// run at stream setup or teardown, when completeness matters most, and
/// stalling together with a wedged terminal is the fail-closed choice —
/// skipping would drop masks (secret unmasked) or misclassify after
/// uncertain bytes (forgery). Every path that runs under a cancellable run
/// must use [`reserve_merged_terminal`] with that run's flag instead so a
/// cancelled run reports instead of hanging.
pub fn reserve_merged_terminal_blocking() -> MergedTerminalReservation {
    let never = AtomicBool::new(false);
    reserve_merged_terminal(&never, None)
        .expect("uncancellable merged-terminal wait has nothing to observe")
}

/// How long a terminal-serialized caller waits with no worker progress before
/// reporting its own cancellation or deadline. The worker checks `stop`
/// between bounded chunks, so a draining reader finishes fast and a slow
/// reader extends the drain via progress; only a wedged pipe (a kernel write
/// blocked with no reader and no progress) reaches this cap. Both the pooled
/// terminal path (`runtime::stdio`) and the direct path below share this
/// bound so every hardened terminal write reports cancellation on the same
/// horizon.
pub(crate) const TERMINAL_CANCEL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Held for the whole outermost finite run. Dropping it releases the
/// process-wide outermost slot claimed by [`enter_finite_run`].
pub struct FiniteRunGuard {
    claimed: bool,
}

impl Drop for FiniteRunGuard {
    fn drop(&mut self) {
        if self.claimed {
            ACTIVE_OUTERMOST_RUNS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Claim the process-wide outermost slot and run the terminal boundary for
/// `registry`: always sweep this run's pre-registered secrets, and reset the
/// shared sanitizers only when no other outermost run is active. The sweep
/// runs before the reset on purpose so the inherited mid-line state tells the
/// first mask whether it must open its own line. Callers must hold the
/// returned guard for the whole run.
///
/// The boundary covers setup only: streaming inside the run still shares the
/// process-global sanitizers, so two outermost runs streaming concurrently
/// interleave their line state (one run's partial line changes the other's
/// line-leading classification) on a best-effort basis. CLI single-run use is
/// unaffected, and nested runs share their parent's terminal stream by design
/// and take no boundary. Library hosts that run concurrent hardened runs
/// against one terminal must serialize those runs for exact filtering.
pub fn enter_finite_run(registry: &crate::manifest::Registry) -> FiniteRunGuard {
    let never = AtomicBool::new(false);
    enter_finite_run_with_cancel(registry, &never, None)
        .expect("uncancellable run boundary has nothing to observe")
}

/// Claim the process-wide outermost slot and run the terminal boundary for
/// `registry` while staying cancellable: the sweep and the reset both wait
/// on the merged-terminal claim with `cancelled`/`deadline`, so a run
/// cancelled while a previous worker is still kernel-blocked reports instead
/// of hanging unbounded. The sweep still precedes the reset, and the reset
/// still waits for in-flight bytes to land before clearing line state, so
/// merged ordering stays fail-closed.
pub fn enter_finite_run_with_cancel(
    registry: &crate::manifest::Registry,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> anyhow::Result<FiniteRunGuard> {
    let _boundary = RUN_BOUNDARY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = ACTIVE_OUTERMOST_RUNS.fetch_add(1, Ordering::SeqCst);
    // If a cancellable boundary fails, release the slot: no run owns it.
    let result = if previous == 0 {
        (|| -> anyhow::Result<()> {
            emit_masks_for_registry_with_cancel(registry, cancelled, deadline)?;
            reset_terminal_state_with_cancel(cancelled, deadline)?;
            Ok(())
        })()
    } else {
        // A concurrent outermost run owns the shared line state: sweeping
        // this run's own secrets is still required, but resetting would drop
        // held split prefixes the other run has not yet been given.
        emit_masks_for_registry_with_cancel(registry, cancelled, deadline)
    };
    if result.is_err() {
        ACTIVE_OUTERMOST_RUNS.fetch_sub(1, Ordering::SeqCst);
    }
    result.map(|()| FiniteRunGuard { claimed: true })
}

/// Emit one mask line per already-registered secret in `registry`. Call after
/// [`enable_if_ci`] (or [`set_enabled`]) when secrets may have been registered
/// before hardening was turned on. Secrets registered afterwards mask
/// themselves through the registry hook; calling this twice only repeats
/// mask lines, which the runner tolerates.
pub fn emit_masks_for_registry(registry: &crate::manifest::Registry) {
    let never = AtomicBool::new(false);
    let _ = emit_masks_for_registry_with_cancel(registry, &never, None);
}

/// Cancellable sweep used on run boundaries that run under a cancel flag.
/// Each mask waits on the merged claim with `cancelled`/`deadline`, so a
/// cancelled run reports instead of hanging behind a wedged worker, while an
/// uncancelled run still stalls together with the terminal (fail-closed) so
/// no mask is skipped and no later filter classifies before prior bytes land.
pub fn emit_masks_for_registry_with_cancel(
    registry: &crate::manifest::Registry,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    if !enabled() {
        return Ok(());
    }
    for secret in crate::security::secret_redactor(registry).snapshot_secrets() {
        emit_mask_for_bytes_with_cancel(&secret, cancelled, deadline)?;
    }
    Ok(())
}

/// Reset the terminal sanitizer line state. The outermost finite-run boundary
/// ([`enter_finite_run`]) calls this after sweeping pre-registered secrets so
/// a trailing partial line (or a held prefix) from a previous run in a
/// long-lived host cannot misclassify the next run's first chunk. The sweep
/// runs first on purpose: the inherited state still tells whether the log is
/// mid-line, so the first mask starts its own line; every mask ends with a
/// newline, so resetting afterwards lands on a true line start. Tests use
/// this between cases for the same reason. Callers must not reset while
/// another run is concurrently streaming to the terminal: the sanitizers are
/// process-global by design — use [`enter_finite_run`], which resets only
/// when no other outermost run is active.
pub fn reset_terminal_state() {
    let never = AtomicBool::new(false);
    let _ = reset_terminal_state_with_cancel(&never, None);
}

/// Cancellable reset that also waits on the merged-terminal claim. After a
/// forced cancel-drain expiry the sink lock is free while the worker still
/// holds the merged claim with bytes in flight; resetting the sanitizers
/// underneath those bytes would misclassify the next run. Waiting on the
/// claim keeps filter order behind landing order. A cancelled waiter reports
/// without touching sanitizer state; the in-flight claim stays owned by its
/// worker (no false unlock), so no later filter can classify early.
pub fn reset_terminal_state_with_cancel(
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    // Take the sink lock outer so a reset cannot slip between another
    // thread's filter and its sink write, then wait on the merged claim so a
    // reset cannot slip between a forced drain expiry and its worker's late
    // bytes landing. Resets only run on run boundaries (no concurrent
    // outermost streaming) or between tests, so the hold is short and never
    // spans a blocking write — unless a worker is still kernel-blocked, in
    // which case the wait reports `cancelled`/deadline instead of clearing
    // state underneath uncertain bytes.
    let _sink = TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _merged = reserve_merged_terminal(cancelled, deadline)?;
    if let Some(slot) = STDOUT_SANITIZER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = StreamSanitizer::new();
        }
    }
    if let Some(slot) = STDERR_SANITIZER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = StreamSanitizer::new();
        }
    }
    Ok(())
}

/// Zero-width space, inserted into a command prefix so the runner no longer
/// recognizes it while the text still reads the same: between the colons of a
/// line-leading `::`, and between the hashes of any `##[` legacy command.
const ZERO_WIDTH_SPACE: &[u8] = "\u{200b}".as_bytes();

/// ASCII whitespace the runner's `TrimStart` skips before a `::` command.
/// `\r` and `\n` are line breaks handled separately; the rest keeps the
/// sanitizer in the line-leading state.
fn is_ascii_leading_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | 0x0B | 0x0C)
}

/// Length of the trailing incomplete UTF-8 sequence in `buf`, if any. ASCII
/// bytes never start such a tail, so a multi-byte whitespace character split
/// across writes is held back until the next chunk completes it.
fn incomplete_tail_len(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let mut trailing_cont = 0usize;
    let mut cursor = buf.len();
    while cursor > 0 && trailing_cont < 3 {
        if buf[cursor - 1] >> 6 == 0b10 {
            trailing_cont += 1;
            cursor -= 1;
        } else {
            break;
        }
    }
    if trailing_cont == 0 {
        let last = buf[buf.len() - 1];
        if last < 0x80 {
            return 0;
        }
        let expected = if last >> 5 == 0b110 {
            2
        } else if last >> 4 == 0b1110 {
            3
        } else if last >> 3 == 0b11110 {
            4
        } else {
            return 0;
        };
        if expected > 1 {
            return 1;
        }
        return 0;
    }
    if cursor == 0 {
        // Only continuation bytes at the end with no leader in this buffer;
        // treat them as invalid bytes that pass through rather than a split.
        return 0;
    }
    let lead = buf[cursor - 1];
    let expected = if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        return 0;
    };
    let have = trailing_cont + 1;
    if have < expected {
        have
    } else {
        0
    }
}

/// Decode the non-ASCII character starting at `index`, if it is a valid UTF-8
/// sequence. Returns the character and its byte length; invalid bytes return
/// `None` so the caller can pass the single byte through.
fn decode_non_ascii(bytes: &[u8], index: usize) -> Option<(char, usize)> {
    let first = bytes[index];
    if first < 0x80 {
        return None;
    }
    let len = if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        return None;
    };
    if index + len > bytes.len() {
        return None;
    }
    let slice = &bytes[index..index + len];
    if slice[1..].iter().any(|byte| byte >> 6 != 0b10) {
        return None;
    }
    std::str::from_utf8(slice)
        .ok()?
        .chars()
        .next()
        .map(|c| (c, len))
}

/// Emit one neutralized modern prefix: `:` + zero-width space + `:`.
fn push_neutralized_colons(out: &mut Vec<u8>) {
    out.push(b':');
    out.extend_from_slice(ZERO_WIDTH_SPACE);
    out.push(b':');
}

/// Emit one neutralized legacy prefix: `#` + zero-width space + `#[`.
fn push_neutralized_hashes(out: &mut Vec<u8>) {
    out.push(b'#');
    out.extend_from_slice(ZERO_WIDTH_SPACE);
    out.push(b'#');
    out.push(b'[');
}

/// Incremental sanitizer for terminal-bound byte streams.
///
/// One-shot [`sanitize_bytes`] treats every buffer as starting a new line,
/// which over-neutralizes when a write continues a previous line and misses a
/// prefix split across two writes. Long-lived terminal streams (streamed step
/// output, final output following streamed output, live diagnostics) share one
/// sanitizer per file descriptor (behind a `Mutex`, so worker threads stay
/// safe) so both cases stay correct. A `::` is neutralized only when nothing
/// but whitespace has appeared since the last `\n`/`\r`, matching the runner's
/// `TrimStart` check; a `##[` legacy command is neutralized wherever it
/// appears, matching the runner's anywhere-in-line search. A line-leading
/// trailing `:` and trailing `#`s that could open `##[` are held back until
/// the next chunk proves what they form, as is a trailing incomplete UTF-8
/// sequence; call [`flush`](Self::flush) at end of stream to release them.
#[derive(Debug, Clone)]
pub struct StreamSanitizer {
    leading: bool,
    pending_colon: bool,
    pending_hashes: u8,
    pending_tail: Vec<u8>,
}

impl StreamSanitizer {
    /// A sanitizer positioned at the start of a log line.
    pub fn new() -> Self {
        Self {
            leading: true,
            pending_colon: false,
            pending_hashes: 0,
            pending_tail: Vec::new(),
        }
    }

    /// Sanitize one chunk, preserving line and prefix state for the next
    /// chunk. Held-back bytes (a line-leading `:`, trailing `#`s, an
    /// incomplete UTF-8 tail) are not emitted until the next chunk or
    /// [`flush`](Self::flush) resolves them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.pending_tail);
        buf.extend_from_slice(bytes);
        let tail = incomplete_tail_len(&buf);
        let mut held_tail = Vec::new();
        if tail > 0 {
            held_tail = buf[buf.len() - tail..].to_vec();
            buf.truncate(buf.len() - tail);
        }
        self.pending_tail = held_tail;
        if self.pending_hashes > 0 {
            let count = usize::from(self.pending_hashes);
            self.pending_hashes = 0;
            let mut prefixed = vec![b'#'; count];
            prefixed.append(&mut buf);
            buf = prefixed;
        }
        if self.pending_colon {
            self.pending_colon = false;
            let mut prefixed = vec![b':'];
            prefixed.append(&mut buf);
            buf = prefixed;
        }
        if buf.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(buf.len() + 4);
        let mut index = 0;
        while index < buf.len() {
            let byte = buf[index];
            if byte == b'\r' || byte == b'\n' {
                out.push(byte);
                self.leading = true;
                index += 1;
                continue;
            }
            if self.leading && is_ascii_leading_ws(byte) {
                out.push(byte);
                index += 1;
                continue;
            }
            if byte < 0x80 {
                if self.leading && byte == b':' {
                    if index + 1 >= buf.len() {
                        // Trailing line-leading `:`: hold it so a `::` split
                        // across chunks is still neutralized.
                        self.pending_colon = true;
                        break;
                    }
                    if buf[index + 1] == b':' {
                        push_neutralized_colons(&mut out);
                        index += 2;
                        self.leading = false;
                        continue;
                    }
                    out.push(b':');
                    index += 1;
                    self.leading = false;
                    continue;
                }
                if byte == b'#' {
                    let remaining = buf.len() - index;
                    if remaining == 1 {
                        self.pending_hashes = 1;
                        break;
                    }
                    if remaining == 2 && buf[index + 1] == b'#' {
                        self.pending_hashes = 2;
                        break;
                    }
                    if remaining >= 3 && buf[index + 1] == b'#' && buf[index + 2] == b'[' {
                        push_neutralized_hashes(&mut out);
                        index += 3;
                        self.leading = false;
                        continue;
                    }
                    out.push(b'#');
                    index += 1;
                    self.leading = false;
                    continue;
                }
                out.push(byte);
                self.leading = false;
                index += 1;
                continue;
            }
            match decode_non_ascii(&buf, index) {
                Some((ch, len)) => {
                    out.extend_from_slice(&buf[index..index + len]);
                    if self.leading {
                        self.leading = ch.is_whitespace();
                    }
                    index += len;
                }
                None => {
                    out.push(byte);
                    self.leading = false;
                    index += 1;
                }
            }
        }
        out
    }

    /// Account for one raw log line written out-of-band, past this sanitizer.
    /// Mask lines must reach the runner literally, so they bypass [`push`](Self::push);
    /// without this call the sanitizer would still believe it is mid-line and
    /// a following line-leading `::` would pass through raw. Held-back bytes
    /// (a split prefix or a partial UTF-8 sequence) are kept: they rejoin the
    /// next chunk, which is now correctly classified as starting a new line.
    pub fn note_external_line(&mut self) {
        self.leading = true;
    }

    /// Whether the log is positioned at the start of a line: every byte the
    /// sanitizer released has been followed by `\n`/`\r`, or nothing was
    /// released yet. Mask emission reads this to decide whether the mask
    /// needs a line break first so the runner recognizes it as its own line.
    pub fn at_line_start(&self) -> bool {
        self.leading
    }

    /// Release bytes held by [`push`](Self::push): a line-leading `:`,
    /// trailing `#`s and an incomplete UTF-8 tail. Returns empty when nothing
    /// is pending.
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.pending_colon {
            self.pending_colon = false;
            out.push(b':');
            self.leading = false;
        }
        if self.pending_hashes > 0 {
            out.extend(std::iter::repeat_n(b'#', usize::from(self.pending_hashes)));
            self.pending_hashes = 0;
            self.leading = false;
        }
        if !self.pending_tail.is_empty() {
            out.extend_from_slice(&self.pending_tail);
            self.pending_tail.clear();
            self.leading = false;
        }
        out
    }
}

impl Default for StreamSanitizer {
    fn default() -> Self {
        Self::new()
    }
}

static STDOUT_SANITIZER: OnceLock<Mutex<StreamSanitizer>> = OnceLock::new();
static STDERR_SANITIZER: OnceLock<Mutex<StreamSanitizer>> = OnceLock::new();

fn stdout_sanitizer() -> &'static Mutex<StreamSanitizer> {
    STDOUT_SANITIZER.get_or_init(|| Mutex::new(StreamSanitizer::new()))
}

fn stderr_sanitizer() -> &'static Mutex<StreamSanitizer> {
    STDERR_SANITIZER.get_or_init(|| Mutex::new(StreamSanitizer::new()))
}

/// Sanitize terminal-bound stdout bytes when hardening is enabled, preserving
/// cross-write line state (including prefixes split across writes). When
/// hardening is off the input is returned unchanged.
///
/// Production terminal writes must hold [`terminal_sink_lock`] across this
/// call and their sink write; calling this without the sink lock classifies
/// correctly only when no concurrent terminal write is in flight.
pub fn filter_stdout(bytes: &[u8]) -> Vec<u8> {
    debug_assert!(
        TERMINAL_SINK_LOCK.try_lock().is_err(),
        "filter_stdout without terminal_sink_lock re-opens the filter/write reorder window"
    );
    if !enabled() {
        return bytes.to_vec();
    }
    stdout_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(bytes)
}

/// Sanitize terminal-bound stderr bytes when hardening is enabled. See
/// [`filter_stdout`] for the cross-write guarantees. Like stdout, production
/// stderr writes must hold the sink lock across filter and write because both
/// streams share the merged runner log.
pub fn filter_stderr(bytes: &[u8]) -> Vec<u8> {
    debug_assert!(
        TERMINAL_SINK_LOCK.try_lock().is_err(),
        "filter_stderr without terminal_sink_lock re-opens the filter/write reorder window"
    );
    if !enabled() {
        return bytes.to_vec();
    }
    stderr_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(bytes)
}

/// Release bytes held by the stdout sanitizer at end of stream (a
/// line-leading `:`, trailing `#`s, an incomplete UTF-8 tail). Returns empty
/// when hardening is off or nothing is pending. Final output paths call this
/// after [`filter_stdout`] so a program whose output ends with a held prefix
/// is not truncated; live streams never flush so a prefix split across writes
/// stays neutralized.
pub fn flush_stdout() -> Vec<u8> {
    debug_assert!(
        TERMINAL_SINK_LOCK.try_lock().is_err(),
        "flush_stdout without terminal_sink_lock can release a prefix a concurrent filter is still classifying"
    );
    if !enabled() {
        return Vec::new();
    }
    stdout_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flush()
}

/// End-of-stream flush for the stderr sanitizer. See [`flush_stdout`].
pub fn flush_stderr() -> Vec<u8> {
    debug_assert!(
        TERMINAL_SINK_LOCK.try_lock().is_err(),
        "flush_stderr without terminal_sink_lock can release a prefix a concurrent filter is still classifying"
    );
    if !enabled() {
        return Vec::new();
    }
    stderr_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flush()
}

/// Sanitize a final stdout buffer at end of stream, preserving the shared
/// cross-write line state with streamed output. Use for program output written
/// after streaming has finished (`run`, `resume`, `--once`, live `host_run`).
///
/// Like [`filter_stdout`], callers must hold [`terminal_sink_lock`] across
/// this call and the sink write that follows; prefer [`write_stdout_final`],
/// which does both atomically.
pub fn filter_stdout_final(bytes: &[u8]) -> Vec<u8> {
    if !enabled() {
        return bytes.to_vec();
    }
    let mut out = filter_stdout(bytes);
    out.extend(flush_stdout());
    out
}

/// End-of-stream sanitize for a final stderr buffer. See [`filter_stdout_final`].
///
/// Like [`filter_stderr`], callers must hold [`terminal_sink_lock`] across
/// this call and the sink write that follows; prefer [`write_stderr_final`],
/// which does both atomically.
pub fn filter_stderr_final(bytes: &[u8]) -> Vec<u8> {
    if !enabled() {
        return bytes.to_vec();
    }
    let mut out = filter_stderr(bytes);
    out.extend(flush_stderr());
    out
}

/// Neutralize workflow-command prefixes in terminal-bound bytes.
///
/// A `::` becomes `:` + zero-width space + `:` when nothing but whitespace
/// has appeared since the buffer start or the last `\n`/`\r`, matching the
/// runner's `TrimStart` check; a `##[` legacy command becomes `#` +
/// zero-width space + `#[` wherever it appears, matching the runner's
/// anywhere-in-line search. Only the opening prefix is broken: the `::`
/// separating a command name from its data stays literal, as do mid-line
/// `::` pairs (Rust paths, URLs, timestamps) and non-UTF-8 sequences, which
/// pass through untouched. Applying the function twice is harmless: the
/// inserted space already breaks the prefix.
pub fn sanitize_bytes(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut leading = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\r' || byte == b'\n' {
            out.push(byte);
            leading = true;
            index += 1;
            continue;
        }
        if leading && is_ascii_leading_ws(byte) {
            out.push(byte);
            index += 1;
            continue;
        }
        if byte < 0x80 {
            if leading && byte == b':' {
                if index + 1 < bytes.len() && bytes[index + 1] == b':' {
                    push_neutralized_colons(&mut out);
                    index += 2;
                    leading = false;
                    continue;
                }
                out.push(b':');
                index += 1;
                leading = false;
                continue;
            }
            if byte == b'#'
                && index + 2 < bytes.len()
                && bytes[index + 1] == b'#'
                && bytes[index + 2] == b'['
            {
                push_neutralized_hashes(&mut out);
                index += 3;
                leading = false;
                continue;
            }
            out.push(byte);
            leading = false;
            index += 1;
            continue;
        }
        match decode_non_ascii(bytes, index) {
            Some((ch, len)) => {
                out.extend_from_slice(&bytes[index..index + len]);
                if leading {
                    leading = ch.is_whitespace();
                }
                index += len;
            }
            None => {
                out.push(byte);
                leading = false;
                index += 1;
            }
        }
    }
    out
}

/// String convenience for [`sanitize_bytes`].
pub fn sanitize_text(text: &str) -> String {
    String::from_utf8_lossy(&sanitize_bytes(text.as_bytes())).into_owned()
}

/// Escape workflow-command data (`::add-mask::` values): `%`, CR and LF.
/// Colons and commas are data here, not separators, and stay literal.
pub fn escape_data(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Format one mask command line (without the trailing newline) for a secret.
pub fn mask_command(secret: &str) -> String {
    format!("::add-mask::{}", escape_data(secret))
}

/// Render mask lines for a snapshot of registered secret bytes. Non-UTF-8
/// values use the same lossy rendering a log viewer would show, so the mask
/// still matches the visible text.
pub fn mask_lines(secrets: &[Vec<u8>]) -> Vec<String> {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .map(|secret| mask_command(&String::from_utf8_lossy(secret)))
        .collect()
}

/// Render the exact bytes to write for one mask line. Callers pass
/// `mid_line=true` (the only mode emission uses) so the mask unconditionally
/// opens its own line with a leading break; the runner's parser requires `::`
/// at a line start after `TrimStart`, otherwise the mask is trailing garbage
/// on the previous line. The returned bytes always hold exactly one
/// `\n`-terminated line and only the escaped secret — never the raw value.
fn mask_line_bytes(line: &str, mid_line: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(line.len() + 2);
    if mid_line {
        buf.push(b'\n');
    }
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    buf
}

/// Write already-filtered terminal bytes on a short-lived worker thread while
/// the caller waits with the same progress-gated cancel drain as the pooled
/// path.
///
/// Direct writers (masks, finals, the live serve notice) hold the sink and
/// the merged-terminal claim across this call. A wedged fd blocks a plain
/// `write_all` in-kernel with no reader, so writing inline ignores a
/// SIGINT/deadline that lands mid-write; the worker instead checks `stop`
/// between bounded chunks and reports progress, while the caller signals
/// `stop` on cancel/deadline and reports after the drain grace when nothing
/// moves. The worker keeps `merged` until its write finishes, so later
/// hardened writers (either descriptor, pooled or direct) still filter behind
/// these bytes even after a forced drain expiry: cross-fd order is preserved
/// by the single flight, not by the released sink lock. A cancelled caller
/// reports without writing anything more; an uncancelled caller without a
/// deadline stalls together with the wedged terminal (fail-closed) instead
/// of skipping bytes or misclassifying behind them.
fn write_direct_bytes_with_cancel(
    to_stdout: bool,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
    merged: MergedTerminalReservation,
) -> std::io::Result<()> {
    if bytes.is_empty() {
        drop(merged);
        if cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("program cancelled by signal"));
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(std::io::Error::other(
                "terminal wait reached its deadline",
            ));
        }
        return Ok(());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(AtomicU64::new(0));
    let (sender, receiver) = mpsc::sync_channel::<std::io::Result<()>>(1);
    let data = bytes.to_vec();
    let worker_stop = Arc::clone(&stop);
    let worker_progress = Arc::clone(&progress);
    std::thread::Builder::new()
        .name("github-actions-direct-write".to_string())
        .spawn(move || {
            let result = write_direct_chunks(to_stdout, &data, &worker_stop, &worker_progress);
            drop(merged);
            let _ = sender.send(result);
        })?;
    // Progress-gated drain mirroring the pooled terminal path: steady worker
    // progress (a slow reader) extends the drain up to the caller deadline,
    // while no progress lets it expire — at the caller deadline, or after
    // `TERMINAL_CANCEL_DRAIN_GRACE` without one — and the cancellation is
    // reported. The worker keeps the merged claim until it finishes, so every
    // later hardened writer still filters behind it after a forced expiry.
    let mut drain_until: Option<Instant> = None;
    let mut last_progress = 0u64;
    loop {
        let cancelled_now = cancelled.load(Ordering::Relaxed);
        let past_deadline = deadline.is_some_and(|end| Instant::now() >= end);
        if cancelled_now || past_deadline {
            stop.store(true, Ordering::Release);
            let now = Instant::now();
            let made = progress.load(Ordering::Acquire);
            if made != last_progress {
                last_progress = made;
                drain_until = Some(match deadline {
                    Some(end) => (now + TERMINAL_CANCEL_DRAIN_GRACE).min(end),
                    None => now + TERMINAL_CANCEL_DRAIN_GRACE,
                });
            } else if drain_until.is_none() {
                drain_until = Some(match deadline {
                    Some(end) if end < now + TERMINAL_CANCEL_DRAIN_GRACE => end,
                    _ => now + TERMINAL_CANCEL_DRAIN_GRACE,
                });
            }
        }
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(result) => {
                // The worker finished: its bytes landed in order. Report any
                // pending cancel/deadline first, else the worker's own result.
                if cancelled.load(Ordering::Relaxed) {
                    return Err(std::io::Error::other("program cancelled by signal"));
                }
                if let Some(end) = deadline {
                    if Instant::now() >= end {
                        return Err(std::io::Error::other(
                            "terminal wait reached its deadline",
                        ));
                    }
                }
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(std::io::Error::other("program cancelled by signal"));
                }
                if deadline.is_some_and(|end| Instant::now() >= end) {
                    return Err(std::io::Error::other(
                        "terminal wait reached its deadline",
                    ));
                }
                return Err(std::io::Error::other(
                    "direct terminal writer stopped without a result",
                ));
            }
        }
        if cancelled.load(Ordering::Relaxed) || deadline.is_some_and(|end| Instant::now() >= end)
        {
            if let Some(until) = drain_until {
                if Instant::now() >= until {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(std::io::Error::other("program cancelled by signal"));
                    }
                    return Err(std::io::Error::other(
                        "terminal wait reached its deadline",
                    ));
                }
            }
        }
    }
}

/// Chunked blocking write for one direct terminal payload on the worker
/// thread owned by [`write_direct_bytes_with_cancel`]: checks `stop` between
/// bounded chunks and reports progress so the caller can extend its drain
/// while a slow reader still moves. A wedged chunk blocks only this thread;
/// the caller still reports its cancel/deadline on a bound.
fn write_direct_chunks(
    to_stdout: bool,
    bytes: &[u8],
    stop: &AtomicBool,
    progress: &AtomicU64,
) -> std::io::Result<()> {
    use std::io::Write as _;
    const CHUNK_BYTES: usize = 8 * 1024;
    let mut offset = 0;
    if to_stdout {
        let mut stdout = std::io::stdout().lock();
        while offset < bytes.len() {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let end = (offset + CHUNK_BYTES).min(bytes.len());
            stdout.write_all(&bytes[offset..end])?;
            offset = end;
            progress.store(offset as u64, Ordering::Release);
        }
        stdout.flush()
    } else {
        let mut stderr = std::io::stderr().lock();
        while offset < bytes.len() {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let end = (offset + CHUNK_BYTES).min(bytes.len());
            stderr.write_all(&bytes[offset..end])?;
            offset = end;
            progress.store(offset as u64, Ordering::Release);
        }
        stderr.flush()
    }
}

/// Direct mid-stream stderr write under the merged-sink discipline, with the
/// same bounded cancel drain as the other direct writers.
///
/// The bytes are filtered under the sink and the claim inside, so
/// classification and landing stay atomic exactly like the pooled path — a
/// concurrent mask cannot land between them. Unlike [`write_stderr_final`]
/// this never flushes the stderr sanitizer: only true end-of-stream
/// diagnostics may flush, since flushing mid-stream would release a held
/// `:` that a following chunk's `:` then completes into a forged
/// line-leading `::`. A cancelled caller reports instead of hanging behind a
/// wedged terminal; the worker keeps the merged claim until its bytes land,
/// so later hardened filters stay ordered behind them.
pub fn write_direct_stderr_with_cancel(
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    let _sink = TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let merged = reserve_merged_terminal(cancelled, deadline)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(std::io::Error::other("program cancelled by signal"));
    }
    if let Some(end) = deadline {
        if Instant::now() >= end {
            return Err(std::io::Error::other(
                "terminal wait reached its deadline",
            ));
        }
    }
    let filtered = if enabled() {
        filter_stderr(bytes)
    } else {
        bytes.to_vec()
    };
    write_direct_bytes_with_cancel(false, &filtered, cancelled, deadline, merged)
}

/// Emit one `::add-mask::` line to standard output for a newly registered
/// secret. Called from the secret registry, so a mask always precedes any
/// later log line that could echo the value. Best-effort and silent when
/// hardening is off or the write fails: masking must never fail a run.
///
/// The mask line bypasses the stdout sanitizer (it must stay a literal
/// workflow command), but it is first synchronized onto its own log line:
/// when either terminal sanitizer is mid-line — for example a previous step
/// streamed `lead` with no trailing newline — a `\n` is written before the
/// mask so the runner recognizes it. Both sanitizers are then resynced to
/// line-start: otherwise a mask landing mid-line would desynchronize them
/// and the next line-leading `::` would pass through raw. Checking both
/// streams covers the merged job log the runner parses (stdout and stderr
/// share its lines); when the descriptors diverge the extra break is at
/// most one blank line in one sink. Held-back split prefixes are kept so
/// they still join the next chunk.
///
/// The merged-sink lock is held across the resync, the mask build, and the
/// `write_all`, in fixed order (sink, then stdout sanitizer, then stderr
/// sanitizer, then the stdout lock). Filtered terminal writes hold the same
/// sink lock across their filter step and their fd write, so neither a
/// partial nor a forged command can slip between the resync and the mask
/// bytes landing. Releasing before the write re-opened both races from the
/// review: a `lead-partial` filtered after the resync could land mid-mask
/// (`\n::add-ma` + `lead-partial ` + `sk::…`, mask lost), and a `::error::`
/// filtered as mid-line before the resync could land after the mask's
/// trailing newline as a line-leading command. Holding across the blocking
/// write means a stalled terminal stalls all hardened terminal output
/// together; that is the fail-closed choice (the descriptors share one
/// runner log, so joint stalling preserves destination semantics). The mask
/// unconditionally opens its own line with a leading `\n`: the shared
/// sanitizer state cannot prove the shared sink is at a line start. The
/// single `write_all` keeps the mask's own bytes contiguous on its fd;
/// already-filtered bytes landing after the mask start a new line because
/// the mask ends with a newline.
pub fn emit_mask_for_bytes(secret: &[u8]) {
    let never = AtomicBool::new(false);
    let _ = emit_mask_for_bytes_with_cancel(secret, &never, None);
}

/// Cancellable mask emission for runs under a cancel flag. Takes the sink
/// lock, then waits on the merged claim with `cancelled`/`deadline`, so a
/// run cancelled while a pooled worker is still kernel-blocked reports
/// instead of hanging unbounded inside the mask. The merged claim keeps the
/// mask ordered behind that worker (fail-closed): on cancellation nothing is
/// resynced or written, so the next filter still classifies behind the late
/// bytes. An uncancelled waiter stalls together with the wedged terminal
/// rather than skipping the mask (secret unmasked) or misclassifying. The
/// write phase itself is bounded too: the bytes go out on a worker thread
/// with the same progress-gated drain as the pooled path, so a SIGINT or
/// deadline that lands mid-write is reported instead of wedging the caller
/// in-kernel.
pub fn emit_mask_for_bytes_with_cancel(
    secret: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    if !enabled() || secret.is_empty() {
        return Ok(());
    }
    let line = mask_command(&String::from_utf8_lossy(secret));
    // Fixed order (sink, merged claim, stdout, stderr): every hardened
    // terminal write nests the same way, so no reverse-order nesting can
    // deadlock. The sink guard spans the drain wait on purpose; see above.
    // The merged claim keeps this mask ordered behind a pooled write whose
    // cancel drain already expired while its worker is still kernel-blocked:
    // without it the mask could resync and land while those uncertain bytes
    // are still in flight. The claim moves into the direct worker below so
    // the ordering survives a forced drain expiry on this side too.
    let _sink = TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let merged = reserve_merged_terminal(cancelled, deadline)?;
    // Re-check after the claim: a cancel that fired in the gap must not
    // resync or write, so the next filter still classifies behind the late
    // bytes (fail-closed, mirroring the pooled path's post-wait check).
    check_merged_cancel(cancelled, deadline)?;
    let bytes = {
        let mut stdout_guard = stdout_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stderr_guard = stderr_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stdout_guard.note_external_line();
        stderr_guard.note_external_line();
        mask_line_bytes(&line, true)
    };
    write_direct_bytes_with_cancel(true, &bytes, cancelled, deadline, merged)
        .map_err(|error| anyhow::anyhow!(error))
}

/// Atomically filter end-of-stream stdout bytes and write them to this
/// process's stdout under the merged-sink lock.
///
/// This is the direct-write counterpart of the pooled terminal path in
/// `runtime::stdio`: the sink guard spans the sanitize step and the blocking
/// `write_all`, so a concurrent mask or streamed chunk cannot interleave
/// between classification and landing. The merged claim additionally orders
/// this write behind a pooled write whose cancel drain expired while its
/// worker is still kernel-blocked. Callers that previously did
/// `filter_stdout_final` followed by `stdout().write_all` must use this so
/// the two steps stay atomic. File redirects stay raw and never call this.
pub fn write_stdout_final(bytes: &[u8]) -> std::io::Result<()> {
    let never = AtomicBool::new(false);
    write_stdout_final_with_cancel(bytes, &never, None)
}

/// Cancellable end-of-stream stdout write. Waits on the merged claim with
/// `cancelled`/`deadline`, so a run cancelled while a pooled worker is still
/// kernel-blocked reports instead of hanging unbounded; on cancellation
/// nothing is filtered or written, preserving merged order (fail-closed).
/// Uncancelled waiters stall together with the wedged terminal. The write
/// phase itself is bounded the same way: the filtered bytes go out on a
/// worker thread with a progress-gated drain, so a SIGINT or deadline that
/// lands mid-write is reported instead of wedging the caller in-kernel while
/// an arbitrary-size program output is still in flight.
pub fn write_stdout_final_with_cancel(
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    let _sink = TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let merged = reserve_merged_terminal(cancelled, deadline)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(std::io::Error::other("program cancelled by signal"));
    }
    if let Some(end) = deadline {
        if Instant::now() >= end {
            return Err(std::io::Error::other(
                "terminal wait reached its deadline",
            ));
        }
    }
    let filtered = if enabled() {
        let mut out = stdout_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes);
        out.extend(
            stdout_sanitizer()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flush(),
        );
        out
    } else {
        bytes.to_vec()
    };
    write_direct_bytes_with_cancel(true, &filtered, cancelled, deadline, merged)
}

/// Atomically filter end-of-stream stderr bytes and write them to this
/// process's stderr under the merged-sink lock: the stdout counterpart of
/// [`write_stdout_final`].
///
/// Without this, a stderr stream ending in a held-back prefix (a
/// line-leading `:`, trailing `#`s, an incomplete UTF-8 tail) is silently
/// truncated — the streaming sanitizer withholds those bytes for the next
/// chunk, and at process exit no next chunk ever comes. Only true
/// end-of-stream stderr writes may use this: flushing mid-stream would
/// release a held `:` that a following chunk's `:` then completes into a
/// forged line-leading `::`. Streaming diagnostics (warnings, progress, the
/// pooled `write_diagnostic` path) must keep using the non-flushing filter so
/// split prefixes stay neutralized across writes.
pub fn write_stderr_final(bytes: &[u8]) -> std::io::Result<()> {
    let never = AtomicBool::new(false);
    write_stderr_final_with_cancel(bytes, &never, None)
}

/// Cancellable end-of-stream stderr write: the stderr counterpart of
/// [`write_stdout_final_with_cancel`]. Waits on the merged claim with
/// `cancelled`/`deadline` so cancellation stays bounded while uncancelled
/// waiters stall together (fail-closed) instead of misclassifying. The write
/// phase itself is bounded the same way via the direct worker below.
pub fn write_stderr_final_with_cancel(
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    let _sink = TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let merged = reserve_merged_terminal(cancelled, deadline)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(std::io::Error::other("program cancelled by signal"));
    }
    if let Some(end) = deadline {
        if Instant::now() >= end {
            return Err(std::io::Error::other(
                "terminal wait reached its deadline",
            ));
        }
    }
    let filtered = if enabled() {
        let mut out = stderr_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes);
        out.extend(
            stderr_sanitizer()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flush(),
        );
        out
    } else {
        bytes.to_vec()
    };
    write_direct_bytes_with_cancel(false, &filtered, cancelled, deadline, merged)
}

/// Test-only shared-sink emission: the same sink-serialized resync-write as
/// [`emit_mask_for_bytes`], but appending to `sink` instead of the process
/// stdout so a concurrent test can observe the merged byte order. The sink,
/// the merged claim, both sanitizer locks, and the sink append are all held
/// in the same fixed order as production, so the test models the production
/// atomicity instead of a stronger in-memory-only guarantee.
#[cfg(test)]
pub(crate) fn emit_mask_to_sink(sink: &Mutex<Vec<u8>>, secret: &[u8]) {
    if !enabled() || secret.is_empty() {
        return;
    }
    let line = mask_command(&String::from_utf8_lossy(secret));
    let _terminal = TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _merged = reserve_merged_terminal_blocking();
    let mut stdout_guard = stdout_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut stderr_guard = stderr_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    stdout_guard.note_external_line();
    stderr_guard.note_external_line();
    let bytes = mask_line_bytes(&line, true);
    if let Ok(mut guard) = sink.lock() {
        guard.extend_from_slice(&bytes);
    }
}

/// Test-only shared-sink filtered write: the production pooled-terminal
/// discipline (sink lock and merged claim across filter and sink append)
/// against `sink`, so adversarial tests model the real worker-pool ordering
/// instead of the old filter-then-release split. Pass `stdout=true` for an
/// fd-1 write and `false` for fd-2.
#[cfg(test)]
pub(crate) fn emit_filtered_to_sink(sink: &Mutex<Vec<u8>>, bytes: &[u8], stdout: bool) -> Vec<u8> {
    let _terminal = TERMINAL_SINK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _merged = reserve_merged_terminal_blocking();
    let filtered = if stdout {
        stdout_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes)
    } else {
        stderr_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes)
    };
    if let Ok(mut guard) = sink.lock() {
        use std::io::Write as _;
        let _ = guard.write_all(&filtered);
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that flip the process-global hardening switch, so
    /// they cannot interleave with the read-only default-off assertion.
    #[cfg(test)]
    static GLOBAL_HARDENING_LOCK: Mutex<()> = Mutex::new(());

    /// Run `body` with the merged-sink lock held, mirroring the production
    /// discipline (filter and sink write stay atomic). Single-threaded tests
    /// wrap direct filter/flush calls in this so the `debug_assert` sink
    /// discipline holds there too. Never wrap [`emit_mask_for_bytes`],
    /// [`emit_mask_to_sink`], [`emit_filtered_to_sink`] or
    /// [`reset_terminal_state`]: they take the sink lock themselves, and the
    /// mutex is not reentrant.
    #[cfg(test)]
    fn with_sink<T>(body: impl FnOnce() -> T) -> T {
        let _sink = terminal_sink_lock();
        body()
    }

    #[test]
    fn hardening_defaults_to_off_without_touching_process_state() {
        // Read-only: this suite never enables hardening, so parallel tests
        // keep legacy behavior.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!enabled());
    }

    #[test]
    fn line_leading_commands_are_neutralized_but_read_the_same() {
        // Only the opening prefix is broken; the `::` before the data stays
        // literal because the runner never reaches it once the prefix fails.
        let out = sanitize_bytes(b"::stop-commands::token\n");
        assert_eq!(out, ":\u{200b}:stop-commands::token\n".as_bytes());
        assert!(!out.starts_with(b"::"));
        // The visible text is unchanged apart from the zero-width break.
        assert_eq!(
            String::from_utf8_lossy(&out).replace('\u{200b}', ""),
            "::stop-commands::token\n"
        );
    }

    #[test]
    fn only_line_leading_pairs_are_rewritten() {
        // Mid-line sequences (Rust paths, URLs, timestamps) are harmless to
        // the runner and must survive byte-identical.
        let input = b"a::b http://x::1\nsecond line ok\n";
        assert_eq!(sanitize_bytes(input), input);
        // The runner trims leading whitespace before checking for `::`, so an
        // indented command is exploitable and must be neutralized.
        assert_eq!(
            sanitize_bytes(b"  ::error::indented\n"),
            "  :\u{200b}:error::indented\n".as_bytes()
        );
        assert_eq!(
            sanitize_bytes(b"\t::warning::tabbed\n"),
            "\t:\u{200b}:warning::tabbed\n".as_bytes()
        );
    }

    #[test]
    fn carriage_returns_and_empty_inputs_pass_through() {
        assert_eq!(sanitize_bytes(b""), b"");
        let crlf = sanitize_bytes(b"done\r\n::error::x\r\n");
        assert_eq!(crlf, "done\r\n:\u{200b}:error::x\r\n".as_bytes());
        let lone_cr = sanitize_bytes(b"a\rb::c");
        assert_eq!(lone_cr, b"a\rb::c");
        let trailing = sanitize_bytes(b"\n::");
        assert_eq!(trailing, "\n:\u{200b}:".as_bytes());
    }

    #[test]
    fn sanitizing_is_idempotent_and_preserves_non_utf8() {
        let once = sanitize_bytes(b"::add-mask::v\nbin\xff\xfe:: mid\n");
        // \xff\xfe survive untouched; the mid-line :: is not at a line start.
        let mut expected = ":\u{200b}:add-mask::v\nbin".as_bytes().to_vec();
        expected.extend_from_slice(&[0xff, 0xfe]);
        expected.extend_from_slice(b":: mid\n");
        assert_eq!(once, expected);
        assert_eq!(sanitize_bytes(&once), once);
        assert_eq!(sanitize_text("::notice::hi"), ":\u{200b}:notice::hi");
    }

    #[test]
    fn mask_commands_escape_percent_and_newlines() {
        assert_eq!(mask_command("hunter2"), "::add-mask::hunter2");
        assert_eq!(mask_command("a%b\r\nc:d,e"), "::add-mask::a%25b%0D%0Ac:d,e");
    }

    #[test]
    fn mask_lines_skip_empties_and_render_lossy_bytes() {
        let lines = mask_lines(&[Vec::new(), b"abc".to_vec(), vec![0xff, 0xfe]]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "::add-mask::abc");
        assert!(lines[1].starts_with("::add-mask::"), "{lines:?}");
    }

    #[test]
    fn stream_sanitizer_neutralizes_a_command_split_across_chunks() {
        // A `:` ending one write and `:` opening the next is one forged
        // command and must still be neutralized.
        let mut stream = StreamSanitizer::new();
        let first = stream.push(b"done\n:");
        assert!(first.ends_with(b"done\n"), "{first:?}");
        let second = stream.push(b":error::x\n");
        assert_eq!(second, ":\u{200b}:error::x\n".as_bytes());
        assert!(stream.flush().is_empty());
    }

    #[test]
    fn stream_sanitizer_keeps_a_lone_line_leading_colon_and_flushes_it() {
        let mut stream = StreamSanitizer::new();
        // A line-leading `:` followed by other text is harmless mid-line.
        let mut out = stream.push(b":ok\n");
        out.extend(stream.push(b"next\n"));
        assert_eq!(out, b":ok\nnext\n");
        // A trailing line-leading `:` is buffered until the stream ends.
        let mut trailing = StreamSanitizer::new();
        assert_eq!(trailing.push(b"value\n:"), b"value\n");
        assert_eq!(trailing.flush(), b":");
        assert!(trailing.flush().is_empty());
    }

    #[test]
    fn stream_sanitizer_preserves_state_across_stream_then_final() {
        // Streamed output ending mid-line followed by final output starting
        // with `::` must not be treated as a line start.
        let mut stream = StreamSanitizer::new();
        let mut out = stream.push(b"partial ");
        out.extend(stream.push(b"::not-a-command\n"));
        assert_eq!(out, b"partial ::not-a-command\n");
        // A streamed newline followed by a final forged command is neutralized.
        let mut boundary = StreamSanitizer::new();
        let mut out = boundary.push(b"first\n");
        out.extend(boundary.push(b"::error::x\n"));
        out.extend(boundary.flush());
        assert_eq!(out, "first\n:\u{200b}:error::x\n".as_bytes());
    }

    #[test]
    fn leading_whitespace_commands_are_neutralized_for_trim_start_runners() {
        // The runner trims spaces and tabs before checking for `::`.
        assert_eq!(
            sanitize_bytes("  ::error::x\n".as_bytes()),
            "  :\u{200b}:error::x\n".as_bytes()
        );
        assert_eq!(
            sanitize_bytes("\t ::notice::hi\n".as_bytes()),
            "\t :\u{200b}:notice::hi\n".as_bytes()
        );
        // A non-breaking space is Unicode whitespace and is trimmed too.
        assert_eq!(
            sanitize_bytes("\u{a0}::error::nbsp\n".as_bytes()),
            "\u{a0}:\u{200b}:error::nbsp\n".as_bytes()
        );
        // Other content before the pair keeps it mid-line and raw.
        assert_eq!(sanitize_bytes(b"x ::error::mid\n"), b"x ::error::mid\n");
        assert_eq!(
            String::from_utf8_lossy(&sanitize_bytes("  ::error::x\n".as_bytes()))
                .replace('\u{200b}', ""),
            "  ::error::x\n"
        );
    }

    #[test]
    fn legacy_hash_commands_are_neutralized_anywhere() {
        // The legacy runner searches for `##[` anywhere in the line, so even
        // mid-line occurrences must break.
        assert_eq!(
            sanitize_bytes(b"##[error]legacy\n"),
            "#\u{200b}#[error]legacy\n".as_bytes()
        );
        assert_eq!(
            sanitize_bytes(b"mid ##[warning]x\n"),
            "mid #\u{200b}#[warning]x\n".as_bytes()
        );
        // Overlapping runs break every `##[` window.
        assert_eq!(sanitize_bytes(b"###[x\n"), "##\u{200b}#[x\n".as_bytes());
        // Single hashes and `#[` pairs are not commands and stay raw.
        assert_eq!(sanitize_bytes(b"# not a command\n"), b"# not a command\n");
        assert_eq!(sanitize_bytes(b"a #[b\n"), b"a #[b\n");
        // Neutralizing twice is stable and the visible text is preserved.
        let once = sanitize_bytes(b"##[error]x\n");
        assert!(!once.windows(3).any(|w| w == b"##["));
        assert_eq!(sanitize_bytes(&once), once);
        assert_eq!(
            String::from_utf8_lossy(&once).replace('\u{200b}', ""),
            "##[error]x\n"
        );
    }

    #[test]
    fn stream_sanitizer_neutralizes_split_whitespace_and_legacy_prefixes() {
        // Leading spaces in one chunk followed by `::` in the next.
        let mut indented = StreamSanitizer::new();
        let mut out = indented.push(b"  ");
        out.extend(indented.push(b"::error::x\n"));
        out.extend(indented.flush());
        assert_eq!(out, "  :\u{200b}:error::x\n".as_bytes());

        // A lone `:` held at a line start still joins a `:` opening the next
        // chunk even after whitespace handling.
        let mut split_colon = StreamSanitizer::new();
        assert_eq!(split_colon.push(b"\n:"), b"\n");
        assert_eq!(
            split_colon.push(b":notice::hi\n"),
            ":\u{200b}:notice::hi\n".as_bytes()
        );

        // Legacy `##[` split as `#` | `#[`, `##` | `[`, and `#` | `#` | `[`.
        let mut split_hash = StreamSanitizer::new();
        let mut out = split_hash.push(b"mid #");
        out.extend(split_hash.push(b"#[warning]x\n"));
        out.extend(split_hash.flush());
        assert_eq!(out, "mid #\u{200b}#[warning]x\n".as_bytes());

        let mut split_bracket = StreamSanitizer::new();
        let mut out = split_bracket.push(b"##");
        out.extend(split_bracket.push(b"[error]y\n"));
        out.extend(split_bracket.flush());
        assert_eq!(out, "#\u{200b}#[error]y\n".as_bytes());

        // A trailing `#` with no command behind it flushes unchanged.
        let mut lone_hash = StreamSanitizer::new();
        assert_eq!(lone_hash.push(b"v#"), b"v");
        assert_eq!(lone_hash.push(b"x\n"), b"#x\n");
        assert!(lone_hash.flush().is_empty());
    }

    #[test]
    fn stream_sanitizer_holds_a_multibyte_whitespace_split() {
        // U+00A0 (C2 A0) split across chunks still counts as leading
        // whitespace, so the following `::` is neutralized.
        let nbsp = "\u{a0}";
        let bytes = nbsp.as_bytes();
        let mut stream = StreamSanitizer::new();
        let mut out = stream.push(&bytes[..1]);
        assert!(out.is_empty());
        out.extend(stream.push(&[bytes[1]]));
        out.extend(stream.push(b"::error::x\n"));
        out.extend(stream.flush());
        let mut expected = nbsp.as_bytes().to_vec();
        expected.extend_from_slice(":\u{200b}:error::x\n".as_bytes());
        assert_eq!(out, expected);
    }

    #[test]
    fn external_line_resyncs_a_mid_line_sanitizer_without_dropping_prefixes() {
        // A mask line lands out-of-band while the log is mid-line: the next
        // line starts over in the terminal, so the sanitizer must too.
        let mut stream = StreamSanitizer::new();
        let mut out = stream.push(b"partial ");
        assert_eq!(out, b"partial ");
        stream.note_external_line();
        out = stream.push(b"::error::after-mask\n");
        assert_eq!(out, ":\u{200b}:error::after-mask\n".as_bytes());
        assert!(stream.flush().is_empty());

        // A held-back split prefix still joins the next chunk after the
        // external line instead of being lost.
        let mut split = StreamSanitizer::new();
        assert_eq!(split.push(b"\n:"), b"\n");
        split.note_external_line();
        assert_eq!(
            split.push(b":notice::hi\n"),
            ":\u{200b}:notice::hi\n".as_bytes()
        );
        assert!(split.flush().is_empty());

        // Trailing `#`s are kept as well: the legacy command they may open
        // is neutralized anywhere in the line regardless.
        let mut hashes = StreamSanitizer::new();
        assert_eq!(hashes.push(b"mid ##"), b"mid ");
        hashes.note_external_line();
        let mut out = hashes.push(b"[warning]x\n");
        out.extend(hashes.flush());
        assert_eq!(out, "#\u{200b}#[warning]x\n".as_bytes());
    }

    #[test]
    fn mask_emission_resyncs_the_shared_stdout_sanitizer() {
        // End-to-end through the process-global path: streamed bytes leave the
        // sanitizer mid-line, a mask line is emitted out-of-band, and a forged
        // command opening the next terminal line is still neutralized.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let streamed = with_sink(|| filter_stdout(b"partial "));
        assert_eq!(streamed, b"partial ");
        // Writes a real `::add-mask::` line to the test harness output; the
        // value is a canary, and the harness captures it per test.
        emit_mask_for_bytes(b"mask-resync-canary-499");
        let forged = with_sink(|| filter_stdout(b"::error::after-mask\n"));
        assert_eq!(forged, ":\u{200b}:error::after-mask\n".as_bytes());
        assert!(with_sink(flush_stdout).is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn terminal_reset_clears_cross_run_line_carryover() {
        // A previous run ending mid-line (or holding a split prefix) must not
        // misclassify the next run's first chunk in a long-lived host.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let _ = with_sink(|| filter_stdout(b"leftover "));
        let _ = with_sink(|| filter_stderr(b"\n:"));
        reset_terminal_state();
        assert_eq!(
            with_sink(|| filter_stdout(b"::error::next-run\n")),
            ":\u{200b}:error::next-run\n".as_bytes()
        );
        assert_eq!(
            with_sink(|| filter_stderr(b"::error::next-run\n")),
            ":\u{200b}:error::next-run\n".as_bytes()
        );
        assert!(with_sink(flush_stdout).is_empty());
        assert!(with_sink(flush_stderr).is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn registry_sweep_is_a_no_op_without_hardening_or_secrets() {
        // The sweep must never emit mask lines (or fail) when hardening is
        // off, so non-CI runs stay byte-identical.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(false);
        let registry = crate::manifest::Registry::default();
        emit_masks_for_registry(&registry);
        assert!(!enabled());
        set_enabled(was);
    }

    #[test]
    fn mask_line_bytes_start_their_own_line_only_when_mid_line() {
        // Pure layout: no global state, no stdout writes.
        let line = mask_command("s3cret%0A");
        assert_eq!(
            mask_line_bytes(&line, false),
            format!("{line}\n").into_bytes()
        );
        assert_eq!(
            mask_line_bytes(&line, true),
            format!("\n{line}\n").into_bytes()
        );
        // Exactly one terminated mask line either way (plus the line break
        // that opens it when mid-line): the runner parses per line, so a
        // mask must never span lines or smuggle a raw newline.
        for mid_line in [false, true] {
            let bytes = mask_line_bytes(&line, mid_line);
            let expected_newlines = if mid_line { 2 } else { 1 };
            assert_eq!(
                bytes.iter().filter(|byte| **byte == b'\n').count(),
                expected_newlines
            );
            assert!(bytes.ends_with(b"\n"));
            assert!(!bytes.contains(&b'\r'));
            let mask_part = bytes.strip_prefix(b"\n".as_slice()).unwrap_or(&bytes);
            assert_eq!(mask_part, format!("{line}\n").into_bytes());
        }
    }

    #[test]
    fn at_line_start_tracks_released_newlines() {
        let mut stream = StreamSanitizer::new();
        assert!(stream.at_line_start());
        let _ = stream.push(b"partial ");
        assert!(!stream.at_line_start());
        let _ = stream.push(b"done\n");
        assert!(stream.at_line_start());
        // Held-back bytes do not move the terminal: a line-leading `:`
        // still counts as a line start until it is released.
        let _ = stream.push(b":");
        assert!(stream.at_line_start());
        assert!(!stream.flush().is_empty());
    }

    #[test]
    fn mask_emission_resyncs_both_shared_sanitizers() {
        // The merged job log shares lines across stdout and stderr: a
        // partial stderr line must break the stdout mask onto its own line
        // too, and both streams must classify the next line as a line start
        // afterwards so forgeries stay neutralized.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let _ = with_sink(|| filter_stdout(b"lead"));
        assert!(!stdout_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .at_line_start());
        // Writes a real `::add-mask::` line to the test harness output; the
        // value is a canary, and the harness captures it per test.
        emit_mask_for_bytes(b"mask-resync-both-499");
        // Both streams resync even though only stdout was mid-line here;
        // either stream mid-line breaks the next mask onto its own line.
        for filtered in with_sink(|| {
            [
                filter_stdout(b"::error::after-mask\n"),
                filter_stderr(b"::warning::after-mask\n"),
            ]
        }) {
            assert!(!filtered.starts_with(b"::"), "{filtered:?}");
        }
        assert_eq!(
            with_sink(|| filter_stdout(b"::error::after-mask\n")),
            ":\u{200b}:error::after-mask\n".as_bytes()
        );
        assert!(with_sink(flush_stdout).is_empty());
        assert!(with_sink(flush_stderr).is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn mask_emission_breaks_after_a_partial_stderr_line() {
        // A partial stderr line leaves the merged log mid-line while stdout
        // alone looks line-started: the next stdout mask must still open a
        // fresh line, and stderr must resync with it.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let _ = with_sink(|| filter_stderr(b"err-partial "));
        // Writes a real `::add-mask::` line to the test harness output; the
        // value is a canary, and the harness captures it per test.
        emit_mask_for_bytes(b"mask-resync-stderr-499");
        assert_eq!(
            with_sink(|| filter_stderr(b"::error::after-mask\n")),
            ":\u{200b}:error::after-mask\n".as_bytes()
        );
        assert!(with_sink(flush_stdout).is_empty());
        assert!(with_sink(flush_stderr).is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn concurrent_mask_emission_never_glues_to_a_shared_sink() {
        // Shared-sink concurrency: stdout/stderr partials and mask emissions
        // race on worker threads while appending to one merged log (the job
        // log the runner parses). Every mask must still open its own line.
        // Both sides hold the merged-sink lock across filter+write (the
        // production pooled-terminal discipline), so neither a partial nor a
        // mask can slip between the other's classify and land steps.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let sink = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
        let start = std::sync::Arc::new(std::sync::Barrier::new(7));
        let mut handles = Vec::new();
        for writer in 0..4 {
            let sink = std::sync::Arc::clone(&sink);
            let start = std::sync::Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                for round in 0..100 {
                    // Sink-serialized like production: filter and append stay
                    // atomic under the terminal lock.
                    if (round + writer) % 2 == 0 {
                        emit_filtered_to_sink(&sink, b"lead-partial ", true);
                    } else {
                        emit_filtered_to_sink(&sink, b"err-partial ", false);
                    }
                }
            }));
        }
        for masker in 0..2 {
            let sink = std::sync::Arc::clone(&sink);
            let start = std::sync::Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                for round in 0..100 {
                    let secret = format!("shared-sink-499-{masker}-{round}");
                    emit_mask_to_sink(&sink, secret.as_bytes());
                }
            }));
        }
        // A concurrent reader keeps the stderr sanitizer hot so masks must
        // observe the merged mid-line state, not just stdout's.
        {
            let sink = std::sync::Arc::clone(&sink);
            let start = std::sync::Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                for _ in 0..100 {
                    emit_filtered_to_sink(&sink, b"err-reader ", false);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker panicked");
        }
        let bytes = sink.lock().map(|guard| guard.clone()).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut mask_lines = 0usize;
        for line in text.lines() {
            if line.contains("::add-mask::") {
                assert!(
                    line.trim_start().starts_with("::add-mask::"),
                    "mask glued mid-line in shared sink: {line:?}"
                );
                mask_lines += 1;
            }
        }
        assert_eq!(
            mask_lines, 200,
            "expected every mask in the sink, got {text:?}"
        );
        // The concurrent storm leaves shared line state mid-line; a reset
        // must still land the next run on a true line start.
        reset_terminal_state();
        assert_eq!(
            with_sink(|| filter_stdout(b"::error::after-storm\n")),
            ":\u{200b}:error::after-storm\n".as_bytes()
        );
        assert!(with_sink(flush_stdout).is_empty());
        assert!(with_sink(flush_stderr).is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn adversarial_sink_serialization_blocks_reordered_forgery() {
        // Deterministic adversarial model of the real worker writers with
        // reordered output. Phase 1 forces the exact split the review
        // reported (filter, stall, mask, late write) with channels only — no
        // sleeps — and proves the sink lock spans filter and write: the mask
        // cannot land until the stalled chunk's bytes have landed. Phase 2
        // storms forged `::error::` chunks against concurrent masks on both
        // streams through barrier rendezvous and asserts the merged log the
        // runner parses never loses a mask and never executes a forgery,
        // whichever order the scheduler chose.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();

        // Phase 1: a stalled filtered write holds the sink from filter to
        // land; a mask racing it must wait for those bytes.
        {
            use std::io::Write as _;
            let sink = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
            let (filtered_tx, filtered_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (attempt_tx, attempt_rx) = std::sync::mpsc::channel::<()>();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let sink_out = std::sync::Arc::clone(&sink);
            let out = std::thread::spawn(move || {
                let _terminal = terminal_sink_lock();
                let filtered = stdout_sanitizer()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(b"lead-partial ");
                // Filtered but not yet written: models a worker stalled
                // before its fd write while still holding the sink.
                let _ = filtered_tx.send(());
                let _ = release_rx.recv();
                if let Ok(mut guard) = sink_out.lock() {
                    let _ = guard.write_all(&filtered);
                }
            });
            // Wait until the output thread has filtered and holds the sink.
            filtered_rx.recv().expect("output thread filtered");
            let sink_mask = std::sync::Arc::clone(&sink);
            let mask = std::thread::spawn(move || {
                let _ = attempt_tx.send(());
                emit_mask_to_sink(&sink_mask, b"adversarial-499");
                let _ = done_tx.send(());
            });
            attempt_rx.recv().expect("mask thread attempted");
            // The mask cannot have completed: the sink is still held from
            // filter to write, so nothing has landed yet either way.
            assert!(
                sink.lock().map(|guard| guard.is_empty()).unwrap_or(false),
                "mask slipped between filter and write"
            );
            assert!(
                done_rx.try_recv().is_err(),
                "mask finished while a filtered write still held the sink"
            );
            let _ = release_tx.send(());
            out.join().expect("output thread panicked");
            done_rx.recv().expect("mask thread finished");
            mask.join().expect("mask thread panicked");
            let bytes = sink.lock().map(|guard| guard.clone()).unwrap_or_default();
            assert_eq!(
                bytes, b"lead-partial \n::add-mask::adversarial-499\n",
                "stalled chunk must land before the mask, on its own line"
            );
        }

        // Phase 2: forged commands race masks on both streams. Every
        // filter+write below is sink-atomic (the production pooled-terminal
        // discipline via `emit_filtered_to_sink` / `emit_mask_to_sink`), so
        // a chunk classified mid-line can only land mid-line (harmless) and
        // a chunk landing after a mask resync classifies line-leading
        // (neutralized). The old filter-release-then-write split allowed the
        // cross combination: classify mid-line, land line-leading.
        reset_terminal_state();
        {
            let sink = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
            emit_filtered_to_sink(&sink, b"lead-partial ", true);
            let start = std::sync::Arc::new(std::sync::Barrier::new(5));
            let mut handles = Vec::new();
            for (worker, chunk, is_stdout) in [
                (0, b"::error::x\n".as_slice(), true),
                (1, b"err-partial ".as_slice(), false),
            ] {
                let sink = std::sync::Arc::clone(&sink);
                let start = std::sync::Arc::clone(&start);
                handles.push(std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..50 {
                        emit_filtered_to_sink(&sink, chunk, is_stdout);
                        let _ = worker;
                    }
                }));
            }
            for masker in 0..2 {
                let sink = std::sync::Arc::clone(&sink);
                let start = std::sync::Arc::clone(&start);
                handles.push(std::thread::spawn(move || {
                    start.wait();
                    for round in 0..50 {
                        let secret = format!("storm-499-{masker}-{round}");
                        emit_mask_to_sink(&sink, secret.as_bytes());
                    }
                }));
            }
            // A fifth peer keeps stderr hot so masks must observe the merged
            // mid-line state, not just stdout's (the cross-fd case: stdout
            // masks vs stderr partials share no kernel ordering, so only the
            // shared sink lock orders the merged log).
            {
                let sink = std::sync::Arc::clone(&sink);
                let start = std::sync::Arc::clone(&start);
                handles.push(std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..50 {
                        emit_filtered_to_sink(&sink, b"err-reader ", false);
                    }
                }));
            }
            for handle in handles {
                handle.join().expect("worker panicked");
            }
            let bytes = sink.lock().map(|guard| guard.clone()).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let mut masks = 0usize;
            for line in text.lines() {
                if line.contains("::add-mask::") {
                    assert!(
                        line.trim_start().starts_with("::add-mask::"),
                        "mask split by reordered output: {line:?}"
                    );
                    masks += 1;
                }
            }
            assert_eq!(masks, 100, "every mask must survive the reorder storm");
            // Runner model: only `::add-mask::` lines may parse; every other
            // line-leading `::` must have been neutralized.
            let mut forged = Vec::new();
            for line in text.lines() {
                if line.contains("::add-mask::") {
                    continue;
                }
                if let Some(rest) = line.trim_start().strip_prefix("::") {
                    forged.push(line.to_owned());
                    let _ = rest;
                }
            }
            assert!(
                forged.is_empty(),
                "reordered output forged a runner command: {forged:?} in {text:?}"
            );
        }
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn concurrent_outermost_boundary_preserves_held_split_prefix() {
        // A second outermost run starting while the first is still active
        // must sweep its own registry without resetting: resetting would
        // drop the held split prefix (`##` below) the first run has not yet
        // been given, and the next chunk would then pass a legacy command
        // through raw.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let registry = crate::manifest::Registry::default();
        let first = enter_finite_run(&registry);
        // Trailing `##` is held back: it may open a `##[` legacy command.
        assert_eq!(with_sink(|| filter_stdout(b"mid ##")), b"mid ");
        let second = enter_finite_run(&registry);
        // No reset happened between the two chunks, so the held prefix
        // rejoins and the legacy command is still neutralized.
        assert_eq!(
            with_sink(|| filter_stdout(b"[warn]\n")),
            "#\u{200b}#[warn]\n".as_bytes()
        );
        assert!(with_sink(flush_stdout).is_empty());
        assert!(with_sink(flush_stderr).is_empty());
        drop(second);
        drop(first);
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn merged_reservation_drop_releases_only_its_own_claim() {
        // The claim is an owned generation token, not a plain boolean: a
        // stale guard (a finished worker racing a newer writer, a reset that
        // already gave up waiting) must never unlock another writer's claim.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owned = try_claim_merged_terminal().expect("claim the merged terminal");
        assert!(try_claim_merged_terminal().is_none());
        drop(MergedTerminalReservation(u64::MAX));
        assert!(
            try_claim_merged_terminal().is_none(),
            "stale reservation drop unlocked a live claim"
        );
        drop(owned);
        assert!(try_claim_merged_terminal().is_some());
    }

    #[test]
    fn cancellable_waiters_report_cancel_without_clearing_an_in_flight_claim() {
        // A held claim models a pooled worker still kernel-blocked after a
        // forced drain expiry: cancelled direct waiters and a cancelled reset
        // must report instead of hanging, and must leave the live claim (and
        // the sanitizer state it protects) untouched — no false unlock.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owned = try_claim_merged_terminal().expect("claim the merged terminal");
        let cancelled = AtomicBool::new(true);
        assert!(reserve_merged_terminal(&cancelled, None).is_err());
        assert!(reset_terminal_state_with_cancel(&cancelled, None).is_err());
        let idle = AtomicBool::new(false);
        assert!(reset_terminal_state_with_cancel(&idle, Some(Instant::now())).is_err());
        assert!(
            try_claim_merged_terminal().is_none(),
            "cancelled waiter released the in-flight claim"
        );
        drop(owned);
        reset_terminal_state_with_cancel(&idle, None).expect("reset after release");
        assert!(try_claim_merged_terminal().is_some());
    }
}
