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
