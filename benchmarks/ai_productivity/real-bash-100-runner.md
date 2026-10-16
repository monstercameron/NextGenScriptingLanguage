# 100-script case runner and package contract

`real_bash_100_runner.py` is the execution/observation layer for a future
frozen 100-task package. It does not dispatch a model, read the upstream
scripts, generate task behavior, or measure performance. Every call handles one
candidate artifact for one task/arm/attempt and keeps a separate immutable
output directory.

The two package files deliberately have different audiences:

* `real-bash-100-task-package.schema.json` describes the one shared package
  handed to all three arms. It contains the user prompt and language-neutral
  behavior brief, with no source path, Bash source, reference implementation,
  functional case, or oracle content. The exact package bytes are hashed once;
  each prompt and behavior brief also has its own exact UTF-8 hash.
* `real-bash-100-case-oracles.schema.json` describes the evaluator-only
  package. It contains owned filesystem cases, invocation inputs, declarative
  external-command protocols, expected process results, expected final
  filesystem trees, and expected mock-command events. It is bound to the exact
  shared brief package bytes. Never expose this package or its directory to a
  programmer session.
* `real-bash-100-attempt-archive-manifest.schema.json` describes the runner's
  per-attempt file integrity manifest. It includes source-code copies so the
  observer's own source hashes can be checked from the archive.

The public package has exactly 100 unique tasks for a study run. A package
marked `fixture_only: true` must use `status: draft_fixture`; it is accepted
only by the focused self-tests or by `validate-packages --allow-fixture` and
its output is labeled `trial_type: fixture`. A real package uses
`fixture_only: false` and `status: frozen_before_generation`. The runner checks
all 100 task IDs across both packages and verifies the public prompt/brief
hashes, the package binding, each per-task oracle hash, and each case contract
before it starts a candidate. Production validation also requires the frozen
hash-only roster and checks every roster task ID, prompt/brief/oracle hash, and
exact criterion ID-to-operation map against the two packages. Every task must declare exactly
one reserved criterion, `all_cases_observed: all_cases_observed`. A candidate
timeout or other non-completed process result therefore cannot pass merely
because its output or filesystem state matches.

For the existing hash-only roster, copy `prompt_sha256` and
`behavior_brief_sha256` from the shared package, `oracle_sha256` from the
corresponding evaluator-only task, and the exact public package file hash into
the roster/freeze. Keep opaque archive references for the per-task oracle and
timing set in the roster. `oracle_sha256` is SHA-256 over canonical UTF-8 JSON
for the stored object `{ "criteria": ..., "cases": ... }`. Canonical JSON
uses sorted keys, compact separators, unescaped Unicode, and no NaN values.
The task hash binds the case arrays exactly as represented in the package;
validation rejects noncanonical base64 and normalizes tree paths before
execution, while the package hash retains the exact file bytes.

Each case starts in a new owned workspace. Files, directories, relative
symlinks, stdin bytes, arguments, and case-specific environment variables are
declared in the oracle package. The runner creates empty `home` and `tmp`
directories for the frozen `HOME` and `TMPDIR` values; include them in the
expected final filesystem tree. The working directory must already exist in
the initial fixture. Symlinks may resolve only within the workspace.

Evaluator-only case templates may use the exact path tokens `{workspace}`,
`{home}`, and `{tmp}` wherever a case string is stored, except in `case_id`.
The runner performs literal substring replacement in arguments, environment
values, mock rules, responses, effects, expected results, and event records;
it does not interpret globs or patterns. In `*_base64` fields it decodes the
canonical bytes, replaces the corresponding ASCII token bytes, then re-encodes
the result. This supports commands that pass a randomly allocated absolute
home or temp path without baking a machine-specific path into the frozen
oracle. A supplied mock `stdin_base64` is matched byte-for-byte after
materialization; if `stdin_sha256` is also supplied, the template's digest must
match its template bytes and the runner recomputes it for the materialized
input. Expected event stdin digests, matched-response digests, and applied
effect digests are derived again from the materialized case.

The stored oracle hash remains the canonical digest of the original
`{criteria, cases}` template. Every attempt archive retains both
`case-template.json` and `case-materialized.json` per case, the exact path
bindings for the case, and each canonical SHA-256 in the observation. The
materialized archive is the validated case actually given to the runner after
exact replacement and schema normalization. Expected mock
event environment values follow the observation convention: workspace paths
are normalized to `$WORKSPACE/...`, so `{home}` there becomes
`$WORKSPACE/home` and `{workspace}/marker` becomes `$WORKSPACE/marker`.

Mocks replace external CLI programs by exact-argv protocol rules. Optionally
declare `observe_env` names per mock to retain those values for every call;
workspace paths are normalized to `$WORKSPACE/...`. For each
invocation, the mock records its command name, argv, exact stdin bytes (as
base64, with a digest), working directory, return status, stdout/stderr
digests, matched rule, and applied fixture effects. An unexpected call returns
status 125 and a clear protocol error, and its event remains in the
observation. Supported effects are file write/append, remove, directory
creation, and rename. The candidate's own
filesystem changes are captured in the final tree as well, so exact state
criteria detect extra files and other side effects.
Only the exact root `.execute-study-runner` directory is hidden from candidate
filesystem snapshots; a same-named nested directory remains visible.

The production CLI requires Linux and an operator-supplied direct-argv sandbox
wrapper. It uses the established `{workspace}` and standalone `{command}`
placeholders from the public development runner. The wrapper must provide its
contained process access to the owned workspace, candidate, mock command
directory, mock socket, frozen candidate runtime, runner Python helper runtime,
and fixed base tools, while withholding the evaluator package directory and
other tasks. Its per-process environment
must retain the runner's mock transport variables and PATH. The runner does
not inspect or certify an arbitrary wrapper and always emits
`sandbox_policy_verified: false`; run production candidates only on an
independently reviewed disposable Linux worker. No such wrapper is installed
or run on the user's Mac.

Example command shape after the independent worker has supplied frozen package
paths, a candidate, runtime argv, and a reviewed wrapper:

```sh
python3 benchmarks/ai_productivity/real_bash_100_runner.py run-attempt \
  --brief-package /worker/locked/shared-briefs.json \
  --oracle-package /evaluator-only/locked/case-oracles.json \
  --roster-package /worker/locked/hash-only-roster.json \
  --task-id task_001 \
  --blind-arm-id arm_abcdefgh \
  --dispatch-index 1 \
  --stage generate \
  --attempt-index 1 \
  --candidate /worker/submission/candidate.sh \
  --runtime-argv-json '["/bin/bash","--noprofile","--norc","{candidate}"]' \
  --runtime-version 'GNU bash 5.x (exact frozen build label)' \
  --sandbox-wrapper-json '["/worker/bin/isolated-runner","--workspace","{workspace}","--","{command}"]' \
  --out /worker/archive/opaque-attempt-id
```

Use a new `--out` directory for every generation or repair attempt. The archive
contains the exact candidate copy, a JSON observation record, raw stdout and
stderr byte files, before/after filesystem snapshots, mock event JSONL, both
case template forms, and `archive-manifest.json`. The manifest lists every
regular file except itself as a sorted relative path, byte count, and file
SHA-256. Its `archive_manifest_sha256` is the SHA-256 of canonical manifest
JSON without the trailing file newline; `observation_sha256` hashes the exact
observation file bytes including its newline. The CLI returns these hashes for
the immutable attempt row. The observation records process status
(`completed`, `launch_error`, `timeout`, `output_limit`, `resource_limit`, or
`infrastructure_error`), the separate filesystem-observation status, exact
return status, byte counts and hashes, the criterion ID/operation map and
results, package hashes, and machine/runtime/runner hashes. Candidate limits
and snapshot node/byte overflows are behavioral outcomes: any bounded-tree
overflow has a false filesystem criterion, status `resource_limit`, and false
`all_cases_observed`. Launch/infrastructure failures leave unavailable
criteria null. If a post-run snapshot fails after process evidence exists, keep
that evidence: known candidate non-completion or resource cap makes completion
false, while a completed process with a missing snapshot leaves completion
null. Case-level details remain in the archive. The observation also
records hashes of the exact runtime argv and wrapper executable. The runner
source hash commits to both this companion module and the existing
`evaluate.py` wrapper-validation/argv-expansion helper it reuses.
Only hashes and declared status are summarized; exact output bytes remain in
the archive. Candidate wall-time values are not reported. A watchdog uses a
fixed timeout to stop a stuck process, but that value is an execution limit,
not a performance observation. The separate frozen timing workload can be run
after functional evaluation using the preregistration's timed protocol.

Safe local validation is limited to the supplied harmless, owned temporary
fixture test:

```sh
python3 benchmarks/ai_productivity/real_bash_100_runner.py validate-packages \
  --brief-package /evaluator/fixture-briefs.json \
  --oracle-package /evaluator/fixture-oracles.json \
  --allow-fixture

python3 tests/ai_productivity_real_bash_100_runner.py
```

The self-test exercises one small Python program written into a temporary
directory plus a fake `fake-cli`; it validates the runner's observations only.
It is not a real task, trial, sample count, source baseline, or productivity
result. This runner creates no model sessions and has not been used to execute
the 100 source files.
