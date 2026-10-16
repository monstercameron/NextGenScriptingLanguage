# Single-queue `recv_any` wait scout

Base commit: `a884f6561fd43f45e43b25a3cc2233be17ee3944`
Worktree: `/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/graph-recv-any-single-queue-a884`

## Product patch

`NodeIo::recv_any` and `recv_any_timeout` currently poll empty queues at 2 ms intervals, even when exactly one active input name and one queue are present. The patch adds a private non-consuming `BoundedQueue::wait_for_change` using that queue's existing condition variable and applies it only to the single-active-queue path. Named `recv_timeout` uses the same bounded wait when its named port has one queue. Multi-name and multi-edge paths keep polling; single-queue receive loops still perform their original scan and preserve seed handling, fairness cursors, EOF checks, cancellation/deadline placement, and zero-timeout behavior. Wait errors are deferred to the next original scan so cancellation retains precedence. Timed waits are capped by both the existing 2 ms polling interval and remaining deadline.

A focused unit test covers zero and positive empty timeouts, ordered receives through `recv_any_timeout` and named `recv_timeout`, EOF, cancellation, and expired graph deadlines. No Rust test, Cargo command, build, benchmark, or timing was run in this scout. `git diff --check` passed.

## E2E benchmark-only patch

The ignored integration harness is a new copy of the previously frozen finite NativeStream graph fixture, with both custom pass-through consumers changed to call `recv_any` on their single bounded Lossless input queue. It times real `GraphSession::start` through `run_to_completion` for one `/bin/cat` producer and two consumers over 4,096 typed records with 256-byte text. Setup/oracle generation stays outside timing. The existing full payload, input/output metadata, sequence ordering, fixture checksum, terminal count, no-drop, and successful producer-status checks run after each timed sample. No source feature flag or new runtime API is introduced. The harness uses its own `EXECUTE_GRAPH_RECV_ANY_E2E_*` environment variables and report format. The potentially large final JSON identity report is written through `BufWriter<File>` and explicitly flushed after serialization; this output work remains outside the timed region.

The paired source arms are now prepared under `work/graph-recv-any-collector-baseline-a884` and `work/graph-recv-any-collector-queue-a884`. Both are detached at `a884f6561fd43f45e43b25a3cc2233be17ee3944` and carry the exact collector-progress patch plus identical harness bytes; only the candidate additionally carries the unchanged queue product patch. This makes the comparison collector-progress versus collector-progress plus single-queue wait, without changing the queue patch base. Exact source and patch bindings are recorded in `paired-source-bindings.json`; future build/run steps are in `paired-build-instructions.md`.

The benchmark patch was generated as a separate additive patch; apply it to both comparison arms identically, then apply the product patch only to the candidate. The original `evidence/graph-transport-e2e` harness and its running evidence were not changed.

## SHA-256

- Product source patch: `3fdb7b11062ebca674b314e046bab9f71ce1656712c231c92a0b18a280c2fa12`
- Benchmark harness patch: `f426bc6535aad17b1a830f4b9a452bafd0fdd311e870100369b48d361c25c38c`
- Benchmark source file: `4294e731012d4f3d28d3deea61e40fcb92a5a4b77ea32c3fc8bdae37f12095e8`
- `src/graph/transport.rs` in this worktree: `8bb5f7ad3b0b1325e82a3714246deec059e50daa128bfc1f6346fe3a72a8d431`
