# 100 model workflows

These `.program` files combine real Unix tools, TypeSafe Jev decisions and
OpenAI generation or strict extraction. Application branches remain in the
program file. Every workflow accepts explicit inputs and writes an output
artifact. Fixtures include exact request expectations and provider responses;
the catalog records byte-exact stdout and output-file oracles.

## Build and run the collection offline

From the repository root:

```sh
cargo build --locked --release
python3 examples/models/run.py --binary target/release/execute --report target/model-workflows.json
```

The runner creates a private workspace per case, copies that workflow's fixtures,
sanitizes inherited credentials, sets `EXECUTE_MODELS_OFFLINE=1`, and checks every
expected result. It uses an OS network sandbox when available and records the
actual isolation mode. Add `--isolate-network` to require OS isolation. No provider
keys or paid model calls are needed. Select a workflow with `--case 1`; selection
still validates the complete catalog and is reported as a partial run.

The cases exercise confident routing, lower-confidence review, confident negative
labels and operation-specific alternatives. A passing fixture means the native
pipeline, provider response parser, typed selection and branch agreed with their
oracle. It does not establish model accuracy, calibration, safe autonomous
judgment, or live output quality. The fixtures are authored tests, not recordings
of 100 live inference runs.

## Run an individual workflow

Paths resolve from the calling directory. To use the checked-in defaults:

```sh
cd examples/models
mkdir -p out
EXECUTE_MODELS_OFFLINE=1 ../../target/release/execute 001-incident-timeline.program --manifest-dir ../../manifests
```

Use the actual filename in the index below if selecting another workflow.
Inspect the interface before supplying your own inputs:

```sh
../../target/release/execute inspect 001-incident-timeline.program
```

## Opt in to real inference

Workflows refuse report paths that alias their existing primary evidence files.
Choose output paths separate from all inputs and credentials.

Each file declares `accept live as boolean with default false`. The default
branch supplies fixture paths. `--input live=true` selects explicit fixtureless
model calls. Unset `EXECUTE_MODELS_OFFLINE` for this deliberate live run and supply
a private credential file:

```sh
../../target/release/execute 001-incident-timeline.program --manifest-dir ../../manifests \
  --input live=true --input env_file=../../.env
```

Provider use may incur charges. Override source paths to process your data and
choose a separate report path. Generation workflows expose `generator_model`,
whose default is `gpt-6-luna`; extraction and other programs may expose a model
input with another name. `execute inspect` shows the exact interface. Inputs and
bounded native preprocessed evidence are sent to the configured provider. Review
those steps before using private data.

Live output may fail the program's domain checks or produce a review artifact;
fixtures do not guarantee live success. Jev acceptance is a confidence threshold,
not a positive class: scripts also test the label, boolean decision, or score.
Generated citations/IDs are checked in selected workflows, but those checks do
not prove the factual correctness of the generated text. Output artifacts remain
for human review; no example commits, pushes, sends messages, edits the operating
system or executes model-suggested commands.

See [model operations](../../docs/models.md) for limits, credential precedence,
strict-schema support, error classes and checkpoint behavior. A failed provider
request may have incurred a charge; checkpoint retry requires explicit
`--retry-uncertain`. There is no automatic retry or model cache.

## Workflow index

The [catalog](catalog.json) is the executable inventory of inputs, cases and exact
outputs. The [100-idea design catalog](../../docs/model-workflow-ideas.md) retains
the broader concepts and rationale; the program files show the implemented scope.

<!-- GENERATED WORKFLOW INDEX -->

| ID | Workflow | Cases |
| --- | --- | --- |
| 001 | [Build an evidence-linked incident timeline](001-incident-timeline.program) | 3 |
| 002 | [Select runbook sections for an alert bundle](002-runbook-selection.program) | 3 |
| 003 | [Review deployment risk from change evidence](003-deployment-risk.program) | 3 |
| 004 | [Compare degradation episodes against a healthy baseline](004-degradation-baseline.program) | 3 |
| 005 | [Prepare a rollback eligibility review packet](005-rollback-review.program) | 3 |
| 006 | [Map change records to service dependency impacts](006-dependency-impact.program) | 3 |
| 007 | [Review candidate duplicate alerts for safe grouping](007-duplicate-alert-review.program) | 3 |
| 008 | [Assemble a cross-shift handoff from local evidence](008-shift-handoff.program) | 3 |
| 009 | [Explain an SLO excursion with bounded hypotheses](009-slo-excursion.program) | 3 |
| 010 | [Generate a tabletop exercise from archived incidents](010-tabletop-exercise.program) | 3 |
| 011 | [Turn a failing-test bundle into a reproduction plan](011-test-reproduction.program) | 3 |
| 012 | [Route incoming issue exports to repository evidence](012-issue-repository-routing.program) | 3 |
| 013 | [Prepare release-note candidates from merged changes](013-release-notes.program) | 3 |
| 014 | [Map an API migration across local clients](014-api-migration-map.program) | 3 |
| 015 | [Cluster static-analysis findings into fix themes](015-static-analysis-themes.program) | 3 |
| 016 | [Assess flaky-test evidence before a rerun policy change](016-flaky-test-policy.program) | 3 |
| 017 | [Find stale documentation examples against captured CLI help](017-stale-doc-examples.program) | 3 |
| 018 | [Assess a dependency upgrade from local evidence](018-dependency-upgrade-review.program) | 3 |
| 019 | [Build an onboarding map for an unfamiliar repository](019-repository-onboarding.program) | 2 |
| 020 | [Explain a regression between two saved test runs](020-regression-comparison.program) | 3 |
| 021 | [Compare contract revisions as an obligation matrix](021-contract-obligations.program) | 2 |
| 022 | [Map policy rules to evidence in local case files](022-policy-evidence-map.program) | 2 |
| 023 | [Compare proposal submissions against fixed criteria](023-proposal-criteria.program) | 2 |
| 024 | [Reconcile meeting minutes against the source transcript](024-meeting-reconciliation.program) | 2 |
| 025 | [Check an RFP response for unsupported claims](025-rfp-claim-review.program) | 2 |
| 026 | [Prepare a cited vendor due-diligence answer packet](026-vendor-due-diligence.program) | 3 |
| 027 | [Create a claim-provenance brief from a technical report](027-claim-provenance-brief.program) | 3 |
| 028 | [Explain invoice exceptions using purchase evidence](028-invoice-exception.program) | 3 |
| 029 | [Index scanned correspondence for human filing](029-correspondence-index.program) | 3 |
| 030 | [Find contradictions and superseded sections in a document set](030-document-supersession-review.program) | 3 |
| 031 | [Triage a local paper library against a research question](031-paper-library-triage.program) | 3 |
| 032 | [Compare support and contradiction for a claim set](032-claim-evidence-matrix.program) | 3 |
| 033 | [Extract reproducibility details from methods sections](033-methods-reproducibility-extraction.program) | 4 |
| 034 | [Reconcile bibliography entries with cited references](034-bibliography-reconciliation.program) | 3 |
| 035 | [Match paper dataset claims to local supplements](035-paper-supplement-match.program) | 3 |
| 036 | [Build a cited timeline from a local historical corpus](036-archival-timeline.program) | 3 |
| 037 | [Answer a question from a closed manual corpus](037-closed-manual-qa.program) | 3 |
| 038 | [Review a standards update against local implementations](038-standards-implementation-review.program) | 3 |
| 039 | [Trace a result across a paper and replication reports](039-paper-replication-trace.program) | 3 |
| 040 | [Code interview excerpts into a traceable qualitative memo](040-qualitative-interview-coding.program) | 3 |
| 041 | [Build a field-aware data dictionary from a raw export](041-raw-export-data-dictionary.program) | 3 |
| 042 | [Map an external feed into a canonical schema](042-external-feed-schema-map.program) | 3 |
| 043 | [Surface minority themes in free-text survey responses](043-survey-minority-themes.program) | 3 |
| 044 | [Investigate conflicting metric totals across reports](044-metric-total-reconciliation.program) | 3 |
| 045 | [Turn validation failures into data-owner review notes](045-validation-owner-review.program) | 3 |
| 046 | [Rank entity-resolution candidates without merging records](046-entity-resolution-candidates.program) | 3 |
| 047 | [Draft field-level lineage from local SQL references](047-sql-field-lineage.program) | 3 |
| 048 | [Diagnose missingness patterns by field and source partition](048-partition-missingness-diagnosis.program) | 3 |
| 049 | [Contextualize statistical outliers against explicit cohorts](049-cohort-outlier-context.program) | 3 |
| 050 | [Detect vocabulary drift between two taxonomies](050-taxonomy-vocabulary-drift.program) | 3 |
| 051 | [Audit document reading order and heading structure](051-document-heading-order-review.program) | 3 |
| 052 | [Draft evidence-bounded alt-text review candidates](052-alt-text-evidence-review.program) | 3 |
| 053 | [Review caption and transcript timing alignment](053-caption-transcript-alignment-review.program) | 3 |
| 054 | [Generate plain-language interface message review](054-plain-language-ui-copy-review.program) | 3 |
| 055 | [Review localization strings for placeholder and plural risks](055-localization-po-review.program) | 3 |
| 056 | [Map PDF and HTML structural findings to review actions](056-document-structure-findings.program) | 3 |
| 057 | [Cluster keyboard test logs into reproducible barriers](057-keyboard-barrier-clusters.program) | 3 |
| 058 | [Check chart narratives against computed table values](058-chart-table-narrative-check.program) | 3 |
| 059 | [Review help and error messages against control inventory](059-help-control-message-join.program) | 3 |
| 060 | [Audit caption and transcript locale coverage](060-caption-locale-coverage.program) | 3 |
| 061 | [Map package vulnerabilities to local runtime usage](061-vulnerability-usage-review.program) | 3 |
| 062 | [Review access-control changes against declared roles](062-access-control-diff-review.program) | 3 |
| 063 | [Review sensitive-data redaction candidates by span ID](063-redaction-span-review.program) | 3 |
| 064 | [Inventory records eligible for retention review](064-retention-eligibility-inventory.program) | 3 |
| 065 | [Map a data-subject request to local source inventory](065-data-request-source-index.program) | 3 |
| 066 | [Compare AI data flows to a supplied policy checklist](066-ai-data-flow-policy-gap-map.program) | 3 |
| 067 | [Trace CI security findings to source evidence](067-ci-security-finding-provenance.program) | 3 |
| 068 | [Review audit event ordering and sequence gaps](068-audit-event-sequence-review.program) | 3 |
| 069 | [Map discovered license notices to attribution inventory](069-license-attribution-map.program) | 3 |
| 070 | [Summarize offline model-assisted fixture regressions](070-model-fixture-regression-review.program) | 3 |
| 071 | [Prepare Downloads folder filing suggestions](071-downloads-filing-suggestions.program) | 3 |
| 072 | [Build a timestamped meeting recording clip index](072-meeting-clip-index.program) | 3 |
| 073 | [Bundle screenshot-to-UI bug evidence from OCR](073-screenshot-ui-bug-evidence.program) | 3 |
| 074 | [Cross-reference warranty, receipt and product records](074-warranty-receipt-product-cross-reference.program) | 3 |
| 075 | [Cluster similar media for human duplicate review](075-similar-media-human-review.program) | 3 |
| 076 | [Answer a troubleshooting question from a closed local knowledge set](076-closed-local-knowledge-answer.program) | 3 |
| 077 | [Find diagnostic-flow gaps in de-identified support cases](077-deidentified-support-diagnostic-gaps.program) | 3 |
| 078 | [Draft evidence-linked support reply macros](078-evidence-linked-support-reply-macro.program) | 3 |
| 079 | [Explain a release change for a specified customer context](079-customer-context-release-explanation.program) | 3 |
| 080 | [Analyze feedback while preserving rare themes](080-versioned-feedback-rare-themes.program) | 3 |
| 081 | [Map student-work evidence to a supplied rubric](081-rubric-evidence-feedback.program) | 3 |
| 082 | [Check syllabus outcomes against readings and assessments](082-syllabus-outcome-coverage.program) | 3 |
| 083 | [Summarize quiz error patterns as misconception candidates](083-quiz-error-misconception-candidates.program) | 3 |
| 084 | [Draft a graded hint ladder from an approved solution](084-approved-solution-hint-ladder.program) | 3 |
| 085 | [Review lab instructions against a supplied safety checklist](085-lab-instruction-checklist-review.program) | 3 |
| 086 | [Audit course materials for declared accessible alternatives](086-course-accessible-alternative-audit.program) | 3 |
| 087 | [Review reading lists against stated prerequisites](087-reading-prerequisite-review.program) | 3 |
| 088 | [Create audience-level variants of an approved FAQ](088-approved-faq-audience-variants.program) | 3 |
| 089 | [Find help-center gaps from anonymized search queries](089-anonymized-help-center-gaps.program) | 3 |
| 090 | [Reconcile escalation handoffs with recorded resolution evidence](090-escalation-handoff-resolution-reconciliation.program) | 3 |
| 091 | [Index field observations by time, location, and source](091-field-observation-time-location-index.program) | 3 |
| 092 | [Reconcile a lab notebook against its recorded protocol](092-notebook-protocol-measurement-reconciliation.program) | 3 |
| 093 | [Build a timestamped index from a supplied audio transcript](093-timestamped-audio-transcript-index.program) | 3 |
| 094 | [Map ecological observations to a supplied taxonomy](094-ecological-observation-taxonomy-review.program) | 3 |
| 095 | [Create a geospatial incident evidence report](095-geospatial-incident-evidence-report.program) | 3 |
| 096 | [Contextualize sensor anomalies against maintenance records](096-sensor-anomaly-maintenance-context.program) | 3 |
| 097 | [Index oral-history themes with timestamps and counterexamples](097-oral-history-theme-counterexample-index.program) | 3 |
| 098 | [Assemble a manufacturing nonconformance evidence packet](098-manufacturing-nonconformance-evidence-packet.program) | 3 |
| 099 | [Review disaster-recovery readiness from saved drill evidence](099-disaster-recovery-saved-drill-readiness.program) | 3 |
| 100 | [Summarize a field survey against its method schema](100-field-survey-method-schema-summary.program) | 3 |
