# Everyday AI gauntlet

These 24 small scripts show where fuzzy interpretation helps ordinary scripting:
understanding paraphrases, extracting records from prose, ranking a TODO, and
explaining saved output. Each accepts `live` as a boolean defaulting to `false`;
offline model calls always supply `fixture`. Each header describes its purpose,
inputs, output artifact, and what bash would need to implement the same task.

The scripts use existing applets and grammar. Each owns its `fixtures/NNN/`
bundle and writes `out/NNN.json`. They do not include another program or require
the authoring tool at runtime. Except for 012's literal Unix `pwd`, they use
in-process applets and the bundled model helper, without jq, a shell, or Python
inside the programs themselves.

## Run every example offline

From the repository root (use `python` on Windows):

```sh
cargo build --offline --release --features full
python3 examples/ai-gauntlet/run.py --binary target/release/execute --report target/ai-gauntlet.json
cargo test --offline --features full --test ai_gauntlet
```

The binary and `execute-model` helper must be adjacent. The harness statically
checks all 24 programs, creates a private working directory for every case,
copies fixtures, removes inherited provider credentials, and sets
`EXECUTE_MODELS_OFFLINE=1`. The helper refuses fixtureless requests before
credentials or network access. The runner has no live-mode option. It compares
byte-exact stdout and output-file oracles from [catalog.json](catalog.json),
checks input fixtures remain unchanged, and rejects evidence of shell evaluation.
Both the Python harness and Rust integration test have a 15-second subprocess
deadline. Program model calls have a 3-second timeout and 65,536-byte limit.

On Windows all 24 sources are checked; 012's Unix-only runtime cases are skipped
with an explicit message and a `cfg(not(unix))` guard in the Rust test. On Unix,
012 additionally tests explicit confirmation, destructive input, a confident
score for a command outside the allowlist, and the numeric review boundary.

To run one default example offline, create `out`, change to this directory, and
use the appropriate binary path:

```sh
mkdir -p out
EXECUTE_MODELS_OFFLINE=1 ../../target/release/execute 002-receipt-json.program --manifest-dir ../../manifests
```

## Contracts and review

Every Jev decision uses a minimum confidence of 80 percent. Scripts read typed
`accepted`, integer `confidence_percent`, and label, decision, or numeric score
fields. They check acceptance separately from the selected label, true/false
answer, or score direction. The batched `jev.ask` examples check both answers'
acceptance and the actionable boolean. A confident negative decision is not
permission. A score equal to the review boundary is withheld.

Generation and extraction follow an evidence-sufficiency judgement (001 instead
classifies error/warn/noise and only summarizes an accepted error). Extraction
uses strict object schemas with all fields required, no extra properties,
bounded text, integer ranges, and closed enums where appropriate, followed by
typed `take` selections. Generation takes text, withholds empty output, and
labels the result for review. Its preflight confidence describes evidence
sufficiency; it does not establish the generated text's correctness.

Low confidence, confident negative answers, malformed provider output, model
refusal, schema rejection and ordinary I/O errors produce the same deterministic
`needs-human-review` record with a null result. Cancellation and the interpreter's
hard limits still stop the run. Test cases also point withheld LLM calls at a
nonexistent fixture, proving those branches do not request a draft.

006 requires explicit `--input confirmed=true` to mark a commit message as a
`confirmed-draft`; it never creates a commit. 005 only proposes config review;
it never applies a change. 012 accepts only the exact source bytes `pwd`, a
confident low danger score, and explicit confirmation before running the
source-code literal `run "pwd" with arguments []`. All other commands are
withheld. Model text only becomes JSON data, including text that contains shell
substitution, backticks, semicolons or ampersands. Suggested diagnostics, tags,
routes, renames, duplicates and appointments remain review artifacts.

Fixtures are authored synthetic provider responses with exact expected requests,
using the conventions of [the operations workflows](../models/README.md).
Passing them establishes pipeline and branching behavior; it does not measure
model accuracy, calibration, factuality or resistance to prompt injection.
Use `python3 examples/ai-gauntlet/author.py` from the repository root to
regenerate programs and fixtures, then run the harness and Rust test.

## Deliberate live use

Individual programs allow `--input live=true`. This is never used by the tests
or harness. It selects fixtureless provider calls using the existing helper's
credential rules. Inspect the program and input before doing this; it can send
source contents to a provider and incur charges. Generation uses `gpt-6-luna`;
Jev uses `jev-latest`. See [model operations](../../docs/models.md). Live failures
are withheld without automatic retries. Use a working directory with an `out`
folder; keep source, schema and credential files outside the fixed output paths.

## Existing contract gaps

- 009 and 012: the grammar has no `take number`. They use `take json` and
  `number.compare`, whose contract rejects nonnumeric operands. No syntax added.
- 002 and 016: the supported schema subset has no `format` or `pattern`, and the
  language has no date/time type. Dates and times are bounded text for human
  review, not certified calendar values. No scheduling is performed.
- 021: there is no basename-specific extraction type. Names remain untrusted
  text in a proposal; a consumer would need its own path policy before renaming.

## Catalog

The machine-readable catalog also contains a one-line `bash_would_need` for
each program; that comparison also appears in every program header.

| Program | Task | Advantage over bash |
| --- | --- | --- |
| [001](001-log-errors.program) | Summarize errors only | Meaning-based severity beats spelling-specific grep rules |
| [002](002-receipt-json.program) | Receipt to JSON | Free-text fields become a strict record with integer cents |
| [003](003-wifi-pick.program) | Pick Wi-Fi | Fuzzy descriptions map to known network IDs |
| [004](004-audio-pick.program) | Pick audio | Everyday device synonyms map to a closed device set |
| [005](005-config-risk.program) | Config risk review | Contextual judgement with a confidence floor |
| [006](006-commit-message.program) | Draft commit message | Diff understanding plus an explicit confirmation gate |
| [007](007-stderr-explain.program) | Explain stderr | Plain-language explanation with a display-only diagnostic |
| [008](008-bug-duplicates.program) | Duplicate bugs | Semantic paraphrases beyond string distance |
| [009](009-todo-urgency.program) | TODO urgency | Context and deadline scoring with a numeric boundary |
| [010](010-download-tag.program) | Download tags | Content meaning instead of filename rules |
| [011](011-manual-search.program) | Man-page search | User intent maps to source-owned snippet IDs |
| [012](012-command-danger.program) | Command danger | Fuzzy danger scoring plus a deterministic execution allowlist |
| [013](013-notification-triage.program) | Notification triage | Timing and actionability checked in one typed batch |
| [014](014-journal-status.program) | Journal status | Short explanation preserves supplied observations |
| [015](015-request-route.program) | Typed request routing | Prose becomes closed routes and bounded integer duration |
| [016](016-appointment-record.program) | Appointment record | Entity extraction without phrase-specific regex |
| [017](017-change-category.program) | Changelog category | User-visible meaning instead of commit prefixes |
| [018](018-download-duplicates.program) | Duplicate downloads | Edition-aware semantic comparison |
| [019](019-backup-exclusions.program) | Backup exclusions | Contextual regenerability judgement for human review |
| [020](020-keybinding-search.program) | Shortcut search | Fuzzy intent matches exact binding IDs |
| [021](021-rename-proposal.program) | Rename proposal | Natural-language names extracted into a strict record |
| [022](022-regex-explain.program) | Explain a regex | Plain-language explanation without a template library |
| [023](023-todo-route.program) | Route a TODO | Route and actionability validated independently in one call |
| [024](024-help-summary.program) | Explain saved help | Intent-aware explanation after an evidence gate |
