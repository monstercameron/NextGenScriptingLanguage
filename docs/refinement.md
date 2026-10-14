# Adversarial refinement, October 13, 2026

The user requested an independent adversarial review and iterative fixes toward
10/10 in every dimension of the supplied [rubric](language-quality-rubric.md).
After discussing the rubric's full-platform requirements, the user chose:
**Keep the Unix glue language small.** Application control flow stays in `.program`;
Rust supplies generic interpreter, process, and widget primitives. This scope
choice does not turn absent platform features into 10/10 capabilities.

## Review method and acceptance

A GPT-6 Luna adversarial subagent read the source and rubric, ran selected tests,
and reproduced defects using owned temporary files, fake applets, and PTYs.
The primary agent implemented fixes and regression tests. The reviewer retested
and inspected the changes, and uncovered a further concurrent stderr bug.
No tests changed Git repositories, sent messages, or wrote remote data.

Scores reflect demonstrated behavior, not how much code changed. A 10 requires
challenging real-world evidence under the original rubric; tests alone do not
establish production readiness, novice comprehension, or AI authoring economics.

## Findings and fixes

| Finding | Final behavior | Evidence |
| --- | --- | --- |
| Script changes a field but next applet receives the old value | Script writes update the current handler's native bindings and widget values in order | UI unit regression |
| Editing a field during a worker mixes old command input with new downstream data | A handler uses one snapshot across its native worker and subsequent actions; its own writes update that snapshot | Pending-worker keyboard-edit regression |
| Missing input silently skips a native step after earlier visible actions | Handler-required input ports are inferred and checked before any action; unavailable inputs defer the entire handler | Deferred-input, unused-input, and headless no-effect regressions |
| Headless mode silently ignores `--max-runs` | Incompatible flags fail before workers start | CLI UI regression |
| Oversized/NUL literals fail only after commands have run | Known invalid port messages and field values fail validation; dynamic writes validate before mutation/delivery | Static no-effect and dynamic atomicity regressions |
| Native command lifetimes are unbounded outside UI | Optional `--command-timeout-ms` bounds each launched pipeline group; UI defaults to 30 seconds | Closed-stdout and live-pipe owned-worker regressions |
| A timeout might permit automatic replay of partial effects | Typed timeout records checkpoint interruption and requires explicit uncertain retry for effectful calls | Timeout/repair/resume regression |
| UI infers timeout from elapsed time and can mislabel another failure | UI retains the actual worker diagnostic; native runtime determines timeout | Source review, existing failure tests |
| Concurrent trace writers race descriptor blocking flags | Flag transitions and writes serialize; lock waiting checks cancellation/deadline | Concurrent full-descriptor deadline regression |
| Cancellation during pipeline construction can launch remaining stages | Cancellation and deadline checks run before each spawn | Source review and existing cancellation tests |
| Public Rust AST bypasses parsed UI size constraints | Runtime validation also enforces placement, component, widget and handler bounds | Source review |

The initial queue-overflow claim was retracted. Valid `.program` components have
at most 32 handlers, and queues coalesce by handler index. Saving outside the
working directory is allowed trusted Unix automation behavior, not a path-escape
regression. It remains incompatible with the original rubric's untrusted sandbox
requirement.

## Small-language operating contracts

- `run` starts with empty stdin; `then` passes a completed value; `pipe to` connects
  native file descriptors concurrently. Final finite output remains buffered.
- All known syntax and manifest errors are checked before applets or saves run.
  Dynamic output errors are checked when data becomes available.
- Each UI component serializes native work. Handler reads use a captured snapshot;
  linked input changes cancel the component's old worker and discard stale results.
- Missing required ports defer the entire handler. Ready handlers preserve queue
  order; repeated pending events coalesce by handler. Timers and subscriptions stay
  declared in `.program`.
- Native process supervision owns process groups, not a security boundary. Arbitrary
  trusted applets retain host permissions. `setsid` escapes are not contained.
- Command deadlines include spawning, native pipe draining and trace writes. They
  do not bound filesystem operations, cache hashing, detached descendants, or final
  stdout delivery. Listener producer lifetime remains script declared.
- Durable recovery retains completed results and refuses uncertain effect retries
  without an explicit flag. External effects are not exactly once.

## Evidence gates that remain open

| Dimension | Acceptance evidence still needed for original 10/10 |
| --- | --- |
| Readability | Independent nonprogrammers correctly explain unfamiliar programs, including pipe/then and event semantics |
| AI generation efficiency | Held-out authoring tasks, fixed model/prompt/catalog, first-pass validity, semantic correctness, token totals and repair cost |
| Declarative expressiveness | Challenging workflows without host-specific orchestration; proposed nested/conditional/full-platform features remain outside small scope |
| Reactive programming | Recorded/replayed adversarial event traces and long-running real apps; UI scheduling currently uses wall-clock ticks |
| Composability | Independent authors successfully integrate explicit contracts; imports/nested reusable UI programs remain outside small scope |
| Type safety | Schema-checked data contracts and projections; ports and result bindings currently remain raw text |
| Error handling | Fault-injection across command, storage and recovery boundaries, plus long-running production evidence; retry policy is explicit/manual |
| Scalability | Large Unix data/event workloads and memory profiles; hundreds of visible UI components remain outside small scope |
| Determinism | Defined replay protocol and reproducible dependency/environment identities; cron follows local timezone and effects depend on external systems |
| Security | Enforced capabilities, sandbox and resource isolation; deliberately absent for trusted local glue |
| Developer experience | Observed authoring/debugging studies and richer diagnostics; autocomplete/hot reload remain unimplemented |
| Runtime efficiency | Matched Rust/shell benchmarks for representative services, allocations/RSS, startup and tail latency; fresh CLI costs remain measurable |

Future rounds must select a concrete in-scope defect or evidence gap, reproduce it,
fix it with a meaningful regression, rerun relevant checks, and obtain independent
re-review. Do not add capability claims or raise a score merely because a round
has no newly reproduced bugs. The original twelve-dimensional 10/10 goal remains
unachieved; large-platform requirements conflict with the later small-language
scope and are explicitly visible here.

## Final independent scorecard

The re-review found no surviving P1/P2 implementation defect in the paths reviewed.
This is a bounded review result, not a claim that the entire interpreter is free
of defects. The reviewer independently passed the selected UI, adversarial,
stdio, and timeout regressions after the fixes.

| Dimension | Score / 10 | Weight |
| --- | ---: | ---: |
| Readability | 7 | 8% |
| AI generation efficiency | 3 | 15% |
| Declarative expressiveness | 5 | 12% |
| Reactive programming | 4 | 10% |
| Composability | 4 | 10% |
| Type safety | 4 | 10% |
| Error handling | 5 | 7% |
| Scalability | 3 | 7% |
| Determinism | 4 | 7% |
| Security | 2 | 6% |
| Developer experience | 4 | 5% |
| Runtime efficiency | 5 | 3% |

The weighted original-rubric result is **4.12/10**. A separate trusted local
Unix automation profile, substituting a security score of 5, gives **4.30/10**.
That profile does not meet the original security definition. Broad architectural
and external-evidence gaps keep the original scores low despite repaired defects.
The initial revised scores also sum to 4.12; the earlier reviewer headline of 4.2
was an arithmetic mismatch and is not retained. The final reviewer increased type
safety, lowered scalability/efficiency against the strict platform criteria, and
left the weighted total unchanged. No dimension is awarded 10/10.

The machine-readable [scorecard](refinement-scorecard.json) records both rounds,
weights, scope, evidence status, and the separate deployment assumption.
