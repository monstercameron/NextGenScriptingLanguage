# Held-out AI productivity evaluation

This directory freezes the evaluation protocol and supplies a standard-library
importer for independently collected results. It contains no model-generated
trial results. `preregistration.json` records the split policy, task-family
holdout rules, success criteria, repair budget, model-parameter capture, the
hash-locked maintainability rubric, blinded scoring, and planned paired
analysis. The sealed task roster, provider
runs, and independent scores are still pending; no AI productivity or
superiority claim is supported yet.

The three workflow arms are Bash, Nushell, and `.program`. The desktop stratum
compares those orchestration arms paired with the same QML/Quickshell target and
desktop acceptance criteria. QML is a UI layer, so QML-only rendering tasks do
not masquerade as shell-language controls. Keep desktop results pending until
the real renderer and disposable Linux desktop runner exist.

## Freeze before trials

An independent evaluator owns a sealed task package matching
`external-package.schema.json`. Before the first model call, assign whole task
families to development, calibration, or held-out splits and freeze the exact
package bytes with `evaluate.py freeze-package`. The task package keeps prompt
bodies and oracle contents out of model/operator view until candidate
submissions are locked. No task family or material variation may cross splits.
The preregistration minima are ten families per stratum for development and
calibration and thirty for held-out evaluation. Export a hash-only roster for
the importer with `evaluate.py export-roster`; it contains no prompt or oracle
body.

Freeze the candidate instructions, references, tool permissions, runtime and
container versions, hidden tests, resource limits, repair messages, and model
settings on calibration data. Randomize language order within task/model blocks.
Start a clean conversation for every trial. Allow one generation and at most
two repairs using the same bounded, language-neutral failure summary. Preserve
every attempt and its usage/cost fields; do not report only the final response.

Each roster task declares mandatory criterion IDs. The external runner applies
the same behavior-level oracle to all language arms: parse/build, output and
status, exact state changes, forbidden side effects, and fixed time/resource
limits as relevant. UI criteria are scored only in the paired desktop stratum.
Two blinded reviewers score the frozen maintainability rubric independently;
a third resolves disagreements. Keep the mapping from random arm IDs to
language/provider/model sealed until candidate hashes and blinded scores are
locked.

## Import and summarize

The result file is JSON Lines, one record per task × blinded arm × replicate,
with every generation/repair attempt and its post-run oracle outcomes. See
`result.schema.json`. Token and cost sources are explicit: estimated price-sheet
costs never become provider-billed costs. Missing usage remains missing. Token
summaries separate provider-reported counts from tokenizer estimates, and the
maintainability summary reports two-rater agreement plus third-rater-resolved
scores when adjudication was required.

```sh
python3 benchmarks/ai_productivity/harness.py validate \
  --roster /secure/evaluator/roster.json \
  --results /secure/evaluator/blind-results.jsonl

python3 benchmarks/ai_productivity/harness.py summarize \
  --roster /secure/evaluator/roster.json \
  --results /secure/evaluator/blind-results.jsonl \
  --arms /secure/evaluator/sealed-arm-map.json \
  --out /secure/evaluator/unblinded-summary.json
```

The arm map is read only after scoring lock and contains the model snapshot,
provider/API, language/toolchain versions, comparison-block identity, decoding
parameters (or explicit `null` when unavailable), prompt hashes, and tool
configuration hash. Summary output labels each row by the blind arm by default;
passing `--arms` explicitly unblinds it. The report includes coverage and
descriptive per-arm metrics. A valid importer run or synthetic fixture is not a
trial. The CLI rejects fixture records, and the unit tests verify schema,
holdout leakage, missing/duplicate cells, attempts, usage, costs, paired
coverage, and fixture rejection.

The sealed arm-map fields are defined separately in `arms.schema.json`; the
exact independent scoring rubric is in `rubric.json`.

## Focused 100-script Bash-behavior recreation study

`preregistration.real-bash-100.json` is a separately versioned protocol for
the thesis's real-script comparison. It leaves the existing
`preregistration.json` unchanged. The target is exactly 100 unique upstream
Bash files, each paired with one task and one fresh session in each of Bash,
Nushell, and Execute: 300 planned sessions, one generation plus at most two
repairs per cell. Each arm recreates source-derived behavior from identical
language-neutral task and behavior briefs. The upstream Bash sources and all
reference implementations are evaluator-only; no programmer arm sees them.
This is not direct source-to-source migration or a migration-acceptance test.
The 100 files are task units, not 100 independent families.
Curators assign functional family IDs, and reports must show the cluster
composition. The current corpus proposal comes from one upstream repository
and release, so results can describe that finite set only.

This focused file is a prospective draft, not an external preregistration or
timestamped protocol freeze. Before dispatch, an independent party must accept
the corpus and timestamp the exact protocol, corpus receipt, full package,
runner, arm map, and randomized schedule. Any changes after dispatch require a
separately versioned amendment and a deviation record.

The corpus begins as `candidate_unsealed`; its candidate manifest, digest
receipt, source selection, task roster, prompts, behavioral briefs, hidden
oracle package, timing cases, runner, language references, arm map, and
randomized plan all need review and exact hashes before dispatch. A corpus
review record is required in the freeze receipt. The source is public and the
Codex programmer surface shares a filesystem, so the protocol cannot promise
pretraining novelty or strong filesystem secrecy. Dispatch one task at a time
with only its assigned language-neutral task and behavior brief; keep the
source, reference implementations, hidden oracle, and other tasks out of that
programmer's working context. Syntax, runtime, reference, and tool differences
by arm must be recorded. A later Execute-versus-original migration check would
be a separate study with a separate acceptance oracle.

The current Codex subagent surface shares a filesystem and is not itself a
suitable isolation boundary. A trial needs 300 clean Luna programmer sessions
inside independently reviewed worker sandboxes that expose only that cell's
task materials and language references; each must be unable to read corpus
sources, other cells, results, or hidden oracles. If this cannot be enforced
by OS/container permissions, do not dispatch the study.

Candidate revision 3 covers the pinned 444-row direct-bin inventory: 439 Bash
files and five non-Bash files. It excludes 20 known exact-source exposures,
five source-reviewed material-adjacency candidates, and eight purpose-only
campaign-scope sources, leaving 406 Bash files before curation. Its fixed-size
convenience sample contains 100 distinct source files in 27 functional
clusters. All 100 tasks belong to one analytic `workflow_cli` stratum; the 30
`command_fixture`, 40 `filesystem_fixture`, and 30
`desktop_cli_simulation` counts are descriptive feasibility quotas, not
independent-family strata. This is separate from the existing
three-stratum/270-task protocol above. The other 306 Bash files were not
behaviorally classified as infeasible. This single-project, single-release
sample cannot support broad population claims.

Revision 1's exact-source disjointness did not establish family-level holdout;
the original adjacency review found nine potentially adjacent families and the
source-only reviewer recorded six direct edges. Revision 2 made five
review-recommended curation swaps. Revision 3 then removes eight purpose-only
campaign-scope workflows and adds eight source-only-reviewed replacements,
preserving 100 unique tasks and the fixture quotas. Revision 2 remains
preserved in the candidate package's history. The independent review artifact
`corpus-revision3-independent-acceptance.json` is retained in the campaign's
external `evidence/` directory; it is not a repository-relative file shipped
with this checkout. That historical review rejects R3 for
held-out admission: tasks `omarchy-057`, `omarchy-051`, `omarchy-076`,
`omarchy-021`, and `omarchy-100` have exposed helper/API or inverse-workflow
overlap. It requests two command-fixture and three filesystem-fixture
replacements, retaining 100 tasks and the 30/40/30 descriptive quotas. R3
remains a verifiable historical candidate; its other 95 rows are not an
accepted roster. Later R4/R5 curation records are separate campaign evidence;
the final R5 candidate has not been accepted. No split, task package, freeze,
or trial is eligible yet.

The versioned R5 protocol amendment is
`preregistration.real-bash-100-r5.json`. It leaves
`preregistration.real-bash-100.json` and the R3 protocol bytes intact, and
preserves the three-stratum/270-task study and R3's 406-eligible, 27-family
frame as historical protocol evidence. R5 describes a later, still-unsealed
candidate: among the same 444 direct-bin inventory rows, it records 20 exact
exposure exclusions, 14 source-reviewed material-adjacency exclusions, and
eight purpose-only campaign-scope exclusions, leaving 397 Bash files. It
selects 100 distinct source/task pairs and leaves 297 eligible files
unselected. The 100 tasks form 26 functional clusters in one analytic
`workflow_cli` stratum. The 30 command, 40 filesystem, and 30 desktop CLI
simulation labels describe fixture feasibility; they are not statistical
strata or independent families.

The R5 source-only review accepts four source swaps for continued unsealed
curation. The separately retained parent integrity receipt checks the pinned
inventory, source copies, counts, and companion digests. Neither artifact
accepts the trial corpus or establishes behavior. R5's purpose-only source
replacement IDs remain historical links into R3; the verifier resolves them
against the preserved R3 manifest and change record, without requiring them
to be active R5 members. R5 also retains the eight purpose-only exclusions
without reading or auditing their source bodies for security. These review
and integrity artifacts are held in the campaign's external corpus/evidence
directories; only their basenames and digests are bound by the amendment.

For R5, `verify-candidate` checks the pinned R5 manifest, receipt, canonical
100-row source map, all selected file bytes and modes, the exact 28-path
companion list and its 27 receipt hashes, the 444-row ledger arithmetic,
reviewed replacement slots, and all 593 files in the preserved R5 snapshot.
The 593-path set must equal the actual regular-file inventory, with only
`history/revision5/SNAPSHOT.json` excluded from its own listing (that descriptor
is separately hash-checked); symlinks and unlisted files are rejected. The
production freeze/review/import gates require revision 5 and the exact pinned
R5 manifest, receipt, and source-map digests. The R3 verifier remains available
for historical integrity checks, but its rejected frame cannot enter an R5
trial import.
It also checks the R2-to-R3-to-R4-to-R5 snapshot/change/review links and the
separate R3 purpose-only history. A successful result means local candidate
integrity only. It does not independently reproduce the upstream inventory,
accept the family split or trial corpus, freeze task briefs/oracles, prove
original behavior, or establish a private held-out split.

The offline `verify-candidate` command checks local manifest/receipt and
canonical source-map consistency, copied bytes, physical line counts and
modes, the 444-row exclusion-ledger arithmetic and selected-source crosswalk,
and every declared/hash-bound exclusion, exposure, similarity, review,
revision-change, and README companion. Its
`candidate_local_integrity_verified_review_pending` status is a local
integrity result, not independent upstream-provenance verification. It does
not fetch the pinned source archive, validate licensing, reproduce the
inventory or exposure search, assess the merit of source-only review, accept
the sample, or assign a held-out split. The receipt's inventory/archive and
licensing verification claims remain curator attestations until independently
reproduced. The separate source-only review is not acceptance of the corpus,
task briefs, fixture feasibility, or experimental split.

```sh
python3 benchmarks/ai_productivity/real_bash_100.py verify-candidate \
  --corpus-dir /secure/evaluator/heldout-bash-corpus
```

`make check` runs synthetic importer and runner contract tests. Four optional
corpus-backed integrity regressions require the separately held candidate;
set `EXECUTE_REAL_BASH_CORPUS=/secure/evaluator/heldout-bash-corpus` when running
`python3 tests/ai_productivity_real_bash_100.py` to include them. These checks
verify evidence bindings and do not execute upstream scripts or admit trials.

The companion case runner observes one submitted artifact; it does not
dispatch Luna sessions or measure candidate duration. The importer binds exact
frozen task, roster, oracle, and runner-source bytes; checks every regular file
against a canonical attempt-archive manifest; rematerializes each case; and
recomputes process, filesystem, mock-command, completion, and aggregate
outcomes from archived raw bytes. A result row whose status or criterion map
differs from that recomputation is rejected. Production imports require the
archive root and both frozen task-package files. Duplicate JSON keys are
rejected in JSON and JSONL inputs.

Archive hashes establish internal byte consistency at import time; they are
not a signature, external timestamp, or write-once guarantee because a
conductor could rewrite both archive and row before import. External archive
custody is still needed. The runner reports `sandbox_policy_verified: false`;
its observations alone do not prove an OS isolation boundary. Do not dispatch
from the shared-filesystem subagent surface. Independent worker-boundary
review and an accepted R4 corpus remain pre-dispatch requirements.

Every task's criterion map includes the reserved
`all_cases_observed: all_cases_observed` operation. A candidate timeout,
output/resource limit, or workspace-snapshot cap therefore fails completion
even if captured output or final files happen to match. If launch or observation
setup fails, unavailable criteria stay null and the cell is incomplete unless
another observed criterion already failed. Provider refusal/timeout rows are
not accepted without a separate provider-event evidence adapter; such cells
remain missing in this importer. The preregistered 1-warm-up/10-repeat runtime
suite is a separate candidate-process measurement after functional pass. The
tooling does not observe per-session token counts, model tool calls, task cost,
or model inference latency. Conductor dispatch-to-response time is reportable
only if an outer conductor records clock boundaries; it includes tools and
scheduling and is not inference time. Human ratings remain pending until
masked-identity review is independently completed and locked.

The focused schemas are `real-bash-100-corpus.schema.json`,
`real-bash-100-receipt.schema.json`,
`real-bash-100-roster.schema.json`, `real-bash-100-arms.schema.json`,
`real-bash-100-run-plan.schema.json`,
`real-bash-100-corpus-review.schema.json`,
`real-bash-100-freeze.schema.json`, and
`real-bash-100-result.schema.json`, plus the companion runner's
`real-bash-100-task-package.schema.json` and
`real-bash-100-case-oracles.schema.json`. Once the accepted corpus and hash-only
roster exist, make a balanced schedule that randomizes script blocks and the
three opaque arm IDs within each block:

```sh
python3 benchmarks/ai_productivity/real_bash_100.py make-plan \
  --roster /secure/evaluator/frozen-roster.json \
  --arms /secure/evaluator/sealed-arms.json \
  --seed 20261015 \
  --out /secure/evaluator/frozen-run-plan.json

python3 benchmarks/ai_productivity/real_bash_100.py validate \
  --freeze /secure/evaluator/freeze.json \
  --corpus-dir /secure/evaluator/heldout-bash-corpus \
  --corpus-review /secure/evaluator/corpus-review.json \
  --roster /secure/evaluator/frozen-roster.json \
  --task-package /secure/evaluator/frozen-task-package.json \
  --oracle-package /secure/evaluator/evaluator-only-oracles.json \
  --plan /secure/evaluator/frozen-run-plan.json \
  --attempt-archive-root /secure/evaluator/attempt-archives \
  --results /secure/evaluator/blind-results.jsonl

python3 benchmarks/ai_productivity/real_bash_100.py summarize \
  --freeze /secure/evaluator/freeze.json \
  --corpus-dir /secure/evaluator/heldout-bash-corpus \
  --corpus-review /secure/evaluator/corpus-review.json \
  --roster /secure/evaluator/frozen-roster.json \
  --task-package /secure/evaluator/frozen-task-package.json \
  --oracle-package /secure/evaluator/evaluator-only-oracles.json \
  --plan /secure/evaluator/frozen-run-plan.json \
  --attempt-archive-root /secure/evaluator/attempt-archives \
  --results /secure/evaluator/blind-results.jsonl \
  --arms /secure/evaluator/sealed-arms.json \
  --out /secure/evaluator/summary.json
```

The run-plan output contains opaque IDs only. Each result row binds to the
freeze and exact dispatch index, preserves every attempt and opaque archive
reference, requires null token/tool-call/cost values, and keeps missing cells
distinct. The importer recomputes included attempts from their archives; the
submitted status and criteria are not evidence on their own. Per-arm output
gives initial/final pass counts over the 100
expected tasks, behavioral-outcome denominators, observed final status counts,
and missing task IDs so infrastructure/refusal/timeout outcomes remain visible.
Repair feedback must equal the failed criterion IDs from the previous attempt.
Runtime is measured by an independent runner only for final
functional passes: one untimed warm-up and ten repetitions against the frozen
timing cases. Paired runtime uses only scripts where both compared arms pass
and have all ten measurements; correctness and the omitted count are shown
beside it. Repair comparisons use only paired behavioral outcomes. Conductor
dispatch-to-response wall time may be reported when directly timed, but it
includes model, tools, and scheduling and is not inference time.

The current Codex Luna surface does not expose normalized per-session token
counters, model tool-call counts, or task-level cost. The focused schema fixes
these as unavailable with null counters; account-level usage and visible-text
token estimates cannot be apportioned as provider usage. Human ratings stay
pending until two reviewers score each rateable final artifact under the
focused rubric, disagreements receive a third score, and a UTC lock time is
recorded. The arm map is opened only after submitted artifacts are locked.
Syntax may reveal the language, so the ratings are masked-identity review,
not guaranteed language-blind review.

`real_bash_100.py` only makes the schedule and validates/imports evidence. It
does not run models, execute candidates, build hidden oracles, certify a
sandbox, review the corpus, or create a freeze receipt. No pilot or full trial
has been run. The three checked-in development fixtures and synthetic unit
fixtures remain test material and do not count toward the 100 real-script
objective. The importer can validate that a submitted row matches the
precommitted schedule and declared hashes; it cannot independently attest that
an external archive reference contains the bytes named by an artifact hash or
that an evaluator executed the hidden oracle honestly.

## Run the public development fixtures

`public-development.json` is a small, deliberately public set of filesystem,
layered-config, and typed-event tasks. It exercises the evaluator itself; it is
not a substitute for the preregistered development/calibration/held-out sample
sizes, and its output is marked `trial_type: fixture` so `harness.py` refuses to
import it as AI evidence. Reference artifacts for Bash, Nushell, and `.program`
are under `public_development/reference_solutions/`.

```sh
python3 benchmarks/ai_productivity/evaluate.py run-public \
  --task filesystem_inventory \
  --candidate-dir bash=benchmarks/ai_productivity/public_development/reference_solutions/bash \
  --candidate-dir nushell=benchmarks/ai_productivity/public_development/reference_solutions/nushell \
  --candidate-dir execute_program=benchmarks/ai_productivity/public_development/reference_solutions/execute_program \
  --trusted-reference-fixtures \
  --execute target/release/execute \
  --out /tmp/filesystem-fixture.json
```

The default is to refuse candidate execution. `--trusted-reference-fixtures`
allows only the checked-in reference directories with their pinned SHA-256
files; it is for validating this harness, not for arbitrary user submissions.
For other candidates, supply `--sandbox-wrapper-json` with a JSON argv array
containing exactly one `{workspace}` (which can be embedded in a policy argument)
and one standalone `{command}` item. The evaluator splices the candidate argv
without invoking a shell. The wrapper must apply a real OS/container isolation
policy and preserve the temporary workspace for the child. The evaluator does
not inspect or certify that policy, and marks wrapper results
`sandbox_policy_verified: false`; use an independently reviewed disposable
runner for held-out trials. `--allow-unsandboxed` exists only as a conspicuous
opt-in for trusted local experiments and runs code with the current user's OS
permissions.

All modes create a fresh temporary fixture workspace for every case, bound
wall time, CPU, per-file size and captured output, and apply an address-space
limit on Linux. The evaluator checks stdout, exact allowed filesystem delta,
output bytes or structured JSON values, and forbidden side effects. These
process limits alone are **not** a security sandbox and do not constrain
process count. The runner never calls a model or records fixture timings as
model generation costs.

The Python tests run Bash reference artifacts against every public case. Once a
release binary or Nushell executable is available, set
`EXECUTE_AI_PRODUCTIVITY_FIXTURE_BINARY` or
`NUSHELL_AI_PRODUCTIVITY_FIXTURE_BINARY` when running
`python3 tests/ai_productivity_evaluate.py` to run those same integration cases
for the corresponding arm.

## Freeze an external held-out package and randomize the schedule

An independent evaluator prepares a private package matching
`external-package.schema.json`. Each task contains its prompt and declarative
oracle, but the package stays sealed from model operators until artifacts are
locked. `freeze-package` validates family splits, task minima, prompt/oracle
commitments, and the seed already recorded in `preregistration.json`; it saves
an immutable sidecar with the exact package byte hash. `plan` verifies the
sidecar and sealed arm map, then produces a deterministic shuffled schedule
within each task/model comparison block. Neither command calls a provider or
puts prompt/oracle bodies in the schedule.

```sh
python3 benchmarks/ai_productivity/evaluate.py freeze-package \
  --package /secure/evaluator/sealed-package.json \
  --seed 20261014 \
  --out /secure/evaluator/sealed-package.freeze.json

python3 benchmarks/ai_productivity/evaluate.py verify-package \
  --package /secure/evaluator/sealed-package.json \
  --freeze /secure/evaluator/sealed-package.freeze.json

python3 benchmarks/ai_productivity/evaluate.py export-roster \
  --package /secure/evaluator/sealed-package.json \
  --freeze /secure/evaluator/sealed-package.freeze.json \
  --out /secure/evaluator/roster.json

python3 benchmarks/ai_productivity/evaluate.py plan \
  --package /secure/evaluator/sealed-package.json \
  --freeze /secure/evaluator/sealed-package.freeze.json \
  --arms /secure/evaluator/sealed-arms.json \
  --out /secure/evaluator/randomized-plan.json
```

Provider execution and independent scoring remain external and pending. The
run plan is a schedule, not an evaluation result. Every attempt's token usage
and cost basis are stored in result records; summaries report input, output,
reasoning, and cached-input token components separately when complete.

Both scripts are intentionally offline. `harness.py` only validates and
summarizes externally collected rows; `evaluate.py` executes checked-in public
development fixtures only. Neither contacts model providers. Independent
providers, secure held-out task execution, blind human review, and actual task
artifacts are required before importing evidence.
