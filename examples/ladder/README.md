# 150 practical Unix utilities in `.program` files

Every numbered file is a working utility with external file/stdin inputs and a
useful report or artifact output. The original 50 syntax exercises were rewritten
as practical tasks; intentionally broken programs are preserved separately as
nine failure/recovery regression fixtures under `tests/fixtures/legacy-ladder`.
They are not counted among the 150 successful utilities.

## Find and run a utility

From the project folder:

```sh
python3 examples/ladder/run.py --list
python3 examples/ladder/run.py --case 76
python3 examples/ladder/run.py --report examples/ladder/results-macos-arm64.json
```

The runner creates a fresh temporary workspace for every utility and alternate
input. It checks the actual `.program` with `execute check`/`explain`, executes it,
and compares stdout and saved files byte for byte with independent catalog
expectations. It checks input preservation and undeclared generated files too.
Each invocation has a deadline and output byte limit. Timing fields are single
observations, not performance benchmarks.

## Use one with your own data

Export a utility and its editable sample inputs into a new or empty folder:

```sh
python3 examples/ladder/run.py --case 76 --prepare /tmp/failed-test-report
cd /tmp/failed-test-report
execute 076-ci-failed-test-summary.program
```

Read `.utility/README.md` and `.utility/contract.json`, replace the sample input
files with your own data in the documented format, then run the same command.
Nested output directories are created by explicit `mkdir` steps in the programs.
Exact sample outputs are available under `.utility/expected/`.

The 144 file/folder utilities now declare 161 path inputs with their existing
paths as defaults. Their prepared guides and `.utility/contract.json` list the
available names. For example:

```sh
execute 076-ci-failed-test-summary.program --input test_results_path=other-results.json
```

The six stdin utilities retain their stream input interfaces. The runner checks
each declared path parameter through the actual CLI, then verifies the existing
default outputs and alternate-input oracles.
The `.program` contains the real application workflow; Python only prepares
fixtures and verifies expected results. File paths resolve from the calling
folder. File outputs may replace the explicitly documented report/artifact path.

Stdin utilities include a `.utility/sample-stdin.txt`; their guide shows how to
pipe it into `execute`. Tail utilities explain how to append records from another
terminal. Interval/calendar utilities stay in the foreground until Ctrl-C; the
test runner supplies a finite run limit. Case 041 waits for a real calendar minute,
so a complete run can wait up to 75 seconds there. Finite streams run through EOF. The deployment verification stream (127)
explicitly stops after its first three deployment records. No OS scheduler entries are installed.

## Tools and input contracts

The runtime invokes actual Unix applets. These tasks use `cat`, `ls`, `tr`,
`grep`, `sort`, `uniq`, `cut`, `head`, `tail`, `wc`, `awk`, `sed`, `comm`, `join`,
`paste`, `mkdir`, and `jq` as needed; each catalog entry lists its tools. Python 3 is only
needed for the test/export runner. Install the required native tools normally;
macOS often needs `jq` supplied separately.

CSV utilities explicitly use **unquoted delimiter-separated records**, not a
complete RFC 4180 parser. Their declared field layouts are inputs, not guesses.
JSON/JSONL utilities document required fields in the program headers and catalog.
Redaction and secret-inventory programs describe their matching policy; they do
not claim universal secret detection. Environment-file export validates variable
names before quoting values. No utility contacts a network service, commits Git
changes, deletes user files, or changes shell/OS configuration.

`catalog.json` contains sample data, exact output oracles, 146 alternate-input
checks and an invalid-input rejection companion. Saved-file expectations are
checked as well as stdout; byte normalization/whitespace stripping is not used.
Program bodies are checked for exact duplicates after quote-aware comment removal.
The collection runs in Cargo's macOS/Linux CI test matrix. Local runtime results
are from macOS; Linux runtime execution requires its CI host.

## Utility index

| IDs | Focus |
| --- | --- |
| 001–025 | Files, support operations, inventory, and text preparation |
| 026–050 | CSV/log reports, caching, stdin handlers, intervals, and calendar apps |
| 051–075 | Configuration, API exports, validation, and deployment inputs |
| 076–100 | CI, release, reliability, capacity, and cost reports |
| 101–125 | Text/CSV operational processing and reconciliation |
| 126–150 | JSONL, persistent streams, event routing, and live audits |

### 001–025

| ID | Utility | Inputs | Outputs |
| --- | --- | --- | --- |
| 001 | [Triage urgent support requests](001-triage-urgent-support-requests.program) | `queue/requests.psv` | `stdout`, `reports/urgent-requests.psv` |
| 002 | [Prepare approved release notes](002-prepare-approved-release-notes.program) | `release/approval.psv` | `stdout`, `reports/release-notes.psv` |
| 003 | [Archive an untrusted text capture literally](003-archive-an-untrusted-text-capture-literally.program) | `intake/clipboard-capture.txt` | `stdout`, `archive/intake/clipboard-capture.txt` |
| 004 | [Export a sorted Japanese glossary](004-export-a-sorted-japanese-glossary.program) | `glossary/master.psv` | `stdout`, `exports/glossary-ja.psv` |
| 005 | [Inventory a selected deliverables folder](005-inventory-a-selected-deliverables-folder.program) | `inventory/target.path`, `workspace/deliverables/current/api-contract.md`, `workspace/deliverables/current/qa-matrix.csv`, `workspace/deliverables/current/release-checklist.txt`, `workspace/deliverables/archive/incident-review.md`, `workspace/deliverables/archive/retrospective.txt` | `stdout`, `reports/deliverables-inventory.txt` |
| 006 | [Assemble a runbook from ordered chapters](006-assemble-a-runbook-from-ordered-chapters.program) | `runbook/00-purpose.md`, `runbook/10-diagnosis.md`, `runbook/20-rollback.md` | `stdout`, `release/runbook.md` |
| 007 | [Copy a binary build artifact](007-copy-a-binary-build-artifact.program) | `build/packages/active.path`, `build/packages/alternate.bundle`, `build/packages/agent.bundle` | `stdout`, `archive/agent.bundle` |
| 008 | [Preview the latest build-log activity](008-preview-the-latest-build-log-activity.program) | `logs/build.log` | `stdout`, `reports/latest-build-activity.log` |
| 009 | [Normalize a unique account-handle list](009-normalize-a-unique-account-handle-list.program) | `access/handles.txt` | `stdout`, `exports/account-handles.txt` |
| 010 | [Filter critical incident records](010-filter-critical-incident-records.program) | `ops/incidents.log` | `stdout`, `reports/sev1-incidents.log` |
| 011 | [Snapshot the selected deployment profile](011-snapshot-the-selected-deployment-profile.program) | `deployment/active-profile.path`, `deployment/profiles/staging.env`, `deployment/profiles/production.env` | `stdout`, `reports/active-profile.snapshot` |
| 012 | [Export enabled service endpoints](012-export-enabled-service-endpoints.program) | `ops/services.json` | `stdout`, `reports/enabled-endpoints.tsv` |
| 013 | [Prepare an invoice audit extract](013-prepare-an-invoice-audit-extract.program) | `finance/invoices.csv` | `stdout`, `reports/invoice-audit.csv` |
| 014 | [Build a meeting action register](014-build-a-meeting-action-register.program) | `meetings/platform-review.txt` | `stdout`, `reports/platform-actions.txt` |
| 015 | [Count a multi-file handoff packet](015-count-a-multi-file-handoff-packet.program) | `handoff/summary.md`, `handoff/open-items.md` | `stdout`, `reports/handoff-line-count.txt` |
| 016 | [List missing deployment dependencies](016-list-missing-deployment-dependencies.program) | `build/dependencies.json` | `stdout`, `reports/missing-dependencies.txt` |
| 017 | [Export enabled feature flags](017-export-enabled-feature-flags.program) | `config/feature-flags.json` | `stdout`, `reports/enabled-feature-flags.txt` |
| 018 | [Sort a supplier contact roster](018-sort-a-supplier-contact-roster.program) | `vendors/contacts.txt` | `stdout`, `exports/supplier-contacts-sorted.csv` |
| 019 | [Convert an unquoted supplier feed to TSV](019-convert-an-unquoted-supplier-feed-to-tsv.program) | `imports/suppliers.csv` | `stdout`, `staging/suppliers.tsv` |
| 020 | [Summarize review queue status counts](020-summarize-review-queue-status-counts.program) | `queue/review-statuses.txt` | `stdout`, `reports/review-status-counts.tsv` |
| 021 | [Preview the first inbound shipments](021-preview-the-first-inbound-shipments.program) | `shipping/inbound.csv` | `stdout`, `reports/inbound-shipment-sample.csv` |
| 022 | [Report overdue undelivered shipments](022-report-overdue-undelivered-shipments.program) | `shipping/tracker.json` | `stdout`, `reports/overdue-shipments.tsv` |
| 023 | [Build a chronological deployment timeline](023-build-a-chronological-deployment-timeline.program) | `deploy/events.psv` | `stdout`, `reports/deployment-timeline.psv` |
| 024 | [Inventory project-designated secret variable names](024-create-a-secret-reference-inventory-without-values.program) | `secrets/runtime.env` | `stdout`, `reports/secret-variable-names.txt` |
| 025 | [Prepare a failed health-check report](025-prepare-a-failed-health-check-report.program) | `quality/check-results.json` | `stdout`, `reports/failed-checks.tsv` |

### 026–050

| ID | Utility | Inputs | Outputs |
| --- | --- | --- | --- |
| 026 | [Build a sorted shortlist of gateway failures](026-json-gateway-incident-shortlist.program) | `gateway.jsonl` | `stdout`, `incidents.tsv` |
| 027 | [Roll up unquoted CSV spend by team](027-team-spend-rollup.program) | `costs.csv` | `stdout`, `team-spend.csv` |
| 028 | [Find approved plugins missing from a deployment](028-approved-plugin-gap-report.program) | `approved.txt`, `installed.txt` | `stdout` |
| 029 | [Join service inventory to its owning team](029-service-owner-inventory.program) | `services.csv`, `owners.csv` | `stdout` |
| 030 | [Summarize a CSV work queue by state](030-work-queue-state-summary.program) | `jobs.csv` | `stdout`, `queue-summary.txt` |
| 031 | [Build a reusable daily SKU index](031-cache-daily-purchase-skus.program) | `purchases.csv` | `stdout` |
| 032 | [Normalize and cache service labels](032-cache-normalized-service-labels.program) | `service-names.txt` | `stdout` |
| 033 | [Cache a CSV selection with parameter-sensitive keys](033-cache-selected-csv-column.program) | `selected-field.txt`, `requests.csv` | `stdout` |
| 034 | [Reuse a fixed runbook preview](034-cache-runbook-preview.program) | `recovery-checklist.txt` | `stdout` |
| 035 | [Route deployment event fields as records arrive](035-deployment-event-routing-feed.program) | `stdin` | `stdout` |
| 036 | [Project JSON audit events into a live TSV feed](036-json-audit-event-feed.program) | `stdin` | `stdout` |
| 037 | [Normalize severity in an authentication alert stream](037-auth-alert-normalizer.program) | `stdin` | `stdout` |
| 038 | [Refresh a one-second work-queue pressure report](038-work-queue-pressure-interval.program) | `jobs.csv` | `stdout` |
| 039 | [Save the latest release error excerpt](039-release-error-excerpt.program) | `release.log` | `stdout`, `release-errors.txt` |
| 040 | [Poll deployment health once per second](040-deployment-health-interval.program) | `deployments.csv` | `stdout` |
| 041 | [Create a capacity report at the next calendar minute](041-calendar-capacity-minute-report.program) | `service-metrics.csv` | `stdout` |
| 042 | [Compare staging and production config values side by side](042-stage-production-config-side-by-side.program) | `staging-config.txt`, `production-config.txt` | `stdout` |
| 043 | [Find duplicate ticket IDs in a CSV import](043-duplicate-ticket-ids.program) | `tickets.csv` | `stdout` |
| 044 | [Redact email addresses in a support export](044-redact-email-addresses.program) | `support-export.txt` | `stdout`, `support-share.txt` |
| 045 | [Collect every API log line for a request ID](045-correlate-api-log-by-request-id.program) | `request-id.txt`, `api.log` | `stdout`, `request-trace.txt` |
| 046 | [Merge release details with workspace context](046-release-handoff-workspace-context.program) | `workspace.txt`, `release.json` | `stdout` |
| 047 | [Rank the most requested web endpoints](047-most-requested-endpoints.program) | `access.log` | `stdout` |
| 048 | [Create a sorted deployment inventory from JSON](048-deployment-inventory-json-report.program) | `deployment.json` | `stdout`, `deployment-inventory.tsv` |
| 049 | [Extract the newest rollback entries from a deployment log](049-latest-rollback-entries.program) | `deploy-history.log` | `stdout`, `rollback-review.txt` |
| 050 | [Follow a live incident log and format each new event](050-tail-live-incident-log-processor.program) | `incidents.log` | `stdout` |

### 051–075

| ID | Utility | Inputs | Outputs |
| --- | --- | --- | --- |
| 051 | [Merge ordered settings layers](051-layered-settings-merge.program) | `fixtures/051/settings.json` | `stdout` |
| 052 | [Redact secret fields recursively](052-recursive-secret-redaction.program) | `fixtures/052/service-response.json` | `stdout` |
| 053 | [List records missing required fields](053-required-field-audit.program) | `fixtures/053/records.json` | `stdout` |
| 054 | [Compare configuration keys and values](054-configuration-key-diff.program) | `fixtures/054/configs.json` | `stdout` |
| 055 | [Flatten API pages into one record array](055-flatten-paginated-api-data.program) | `fixtures/055/pages.json` | `stdout` |
| 056 | [Audit environment variable declarations](056-environment-manifest-audit.program) | `fixtures/056/environment-manifest.json` | `stdout` |
| 057 | [Render JSON values as shell assignments](057-json-environment-export.program) | `fixtures/057/environment.json` | `stdout`, `runtime.env.sh` |
| 058 | [Export deployment services as TSV](058-deployment-inventory-tsv.program) | `fixtures/058/deployment.json` | `stdout`, `services.tsv` |
| 059 | [Flatten nested settings to dotted paths](059-flatten-config-path-report.program) | `fixtures/059/config.json` | `stdout` |
| 060 | [Preview notification destinations](060-notification-routing-preview.program) | `fixtures/060/notifications.json` | `stdout` |
| 061 | [Render a target deployment plan](061-deployment-config-render.program) | `fixtures/061/release.json` | `stdout`, `deployment-plan.json` |
| 062 | [Extract actionable API failures](062-api-error-report.program) | `fixtures/062/responses.json` | `stdout` |
| 063 | [Group dependency inventory by license](063-dependency-license-inventory.program) | `fixtures/063/packages.json` | `stdout` |
| 064 | [Summarize API latency by route](064-api-latency-summary.program) | `fixtures/064/requests.json` | `stdout` |
| 065 | [Normalize user records from API versions](065-normalize-api-users.program) | `fixtures/065/users.json` | `stdout` |
| 066 | [Compare declared and provided environment values](066-environment-contract-diff.program) | `fixtures/066/environment.json` | `stdout` |
| 067 | [Review dependency inventory changes](067-dependency-version-diff.program) | `fixtures/067/lock-comparison.json` | `stdout` |
| 068 | [Audit page counts and duplicate record IDs](068-pagination-integrity-audit.program) | `fixtures/068/paged-export.json` | `stdout` |
| 069 | [Export records as newline-delimited JSON](069-jsonl-record-export.program) | `fixtures/069/export.json` | `stdout`, `events.jsonl` |
| 070 | [Find endpoints missing granted token scopes](070-api-scope-audit.program) | `fixtures/070/scopes.json` | `stdout` |
| 071 | [Split records into bounded request batches](071-api-batch-request-builder.program) | `fixtures/071/batch.json` | `stdout` |
| 072 | [Summarize API events by service and kind](072-api-event-summary.program) | `fixtures/072/events.json` | `stdout` |
| 073 | [Calculate API rate-limit budgets](073-api-rate-limit-budget.program) | `fixtures/073/rate-limits.json` | `stdout` |
| 074 | [Resolve duplicate API records by update time](074-deduplicate-api-records.program) | `fixtures/074/records.json` | `stdout` |
| 075 | [Preview effective read access from policy grants](075-effective-read-access-preview.program) | `fixtures/075/access-policy.json` | `stdout` |

### 076–100

| ID | Utility | Inputs | Outputs |
| --- | --- | --- | --- |
| 076 | [Group failed tests by suite](076-ci-failed-test-summary.program) | `reports/test-results.json` | `stdout` |
| 077 | [Rank the slowest CI tests](077-ci-slowest-tests.program) | `reports/test-timings.json` | `stdout` |
| 078 | [Prioritize uncovered source files](078-coverage-largest-gaps.program) | `reports/coverage.json` | `stdout` |
| 079 | [Summarize release artifacts by kind](079-release-artifact-inventory.program) | `release/artifact-inventory.json` | `stdout` |
| 080 | [Group failed build-matrix jobs](080-ci-build-matrix-failures.program) | `ci/matrix-results.json` | `stdout` |
| 081 | [Rank workflow jobs by duration](081-ci-job-duration-report.program) | `ci/job-durations.json` | `stdout` |
| 082 | [Format exported release checksums](082-release-checksum-manifest.program) | `release/checksums.json` | `stdout` |
| 083 | [Report dependencies with newer versions](083-dependency-outdated-report.program) | `dependency/updates.json` | `stdout` |
| 084 | [Count findings by severity](084-security-severity-counts.program) | `security/findings.json` | `stdout` |
| 085 | [Calculate available CI runner capacity](085-runner-fleet-capacity.program) | `ci/runner-pools.json` | `stdout` |
| 086 | [Calculate per-route p95 latency](086-route-latency-p95.program) | `observability/latencies.json` | `stdout` |
| 087 | [Count operational events by day and type](087-daily-event-counts.program) | `operations/events.json` | `stdout` |
| 088 | [Aggregate cloud charges by team](088-team-cost-summary.program) | `finance/cloud-charges.json` | `stdout` |
| 089 | [Reconcile invoice line extensions](089-invoice-line-reconciliation.program) | `finance/invoices.json` | `stdout` |
| 090 | [Rank cost centers by budget variance](090-budget-versus-actual.program) | `finance/budget-close.json` | `stdout` |
| 091 | [List changed files from a deployment export](091-deployment-changed-files.program) | `release/deployment-diff.json` | `stdout` |
| 092 | [Check required release gates](092-release-required-gates.program) | `release/gates.json` | `stdout` |
| 093 | [Identify tests with mixed outcomes](093-flaky-test-rates.program) | `ci/repeated-tests.json` | `stdout` |
| 094 | [List quarantines expiring soon](094-expiring-test-quarantines.program) | `ci/quarantines.json` | `stdout` |
| 095 | [Rank growing release artifacts](095-artifact-size-regressions.program) | `release/size-comparison.json` | `stdout` |
| 096 | [Find services below rollout capacity](096-rollout-health.program) | `deploy/rollout-status.json` | `stdout` |
| 097 | [Summarize failed webhook deliveries](097-webhook-delivery-failures.program) | `operations/webhook-deliveries.json` | `stdout` |
| 098 | [List incidents past their response SLA](098-incident-sla-overdue.program) | `operations/incidents.json` | `stdout` |
| 099 | [Rank remaining SLO error budgets](099-slo-error-budget-remaining.program) | `observability/error-budgets.json` | `stdout` |
| 100 | [Project month-end cloud spend](100-monthly-cloud-cost-forecast.program) | `finance/monthly-spend.json` | `stdout` |

### 101–125

| ID | Utility | Inputs | Outputs |
| --- | --- | --- | --- |
| 101 | [Keep the first contact row for each email address](101-deduplicate-contacts-by-email.program) | `contacts.csv` | `stdout` |
| 102 | [Value warehouse stock from a sorted price book](102-value-stock-from-price-book.program) | `stock.csv`, `prices.csv` | `stdout` |
| 103 | [Count contact addresses by email domain](103-count-contact-email-domains.program) | `directory.csv` | `stdout` |
| 104 | [Build a privacy-minimized staff import](104-project-private-staff-columns.program) | `staff.csv` | `stdout` |
| 105 | [Count application log records by severity](105-count-log-severity-levels.program) | `service.log` | `stdout` |
| 106 | [Summarize ingress requests by HTTP method](106-count-http-method-volume.program) | `requests.tsv` | `stdout` |
| 107 | [Report stock below reorder points](107-alert-on-reorder-shortfall.program) | `reorder.csv` | `stdout` |
| 108 | [Normalize CRLF text files to LF](108-normalize-crlf-text.program) | `vendor-notes.txt` | `stdout` |
| 109 | [Strip terminal color codes from a saved build log](109-strip-terminal-color-codes.program) | `colored-build.log` | `stdout` |
| 110 | [Exclude opted-out addresses from a recipient export](110-exclude-suppressed-mail-recipients.program) | `recipients.csv`, `suppressed.csv` | `stdout` |
| 111 | [Total worked shift minutes by employee](111-total-worked-minutes-by-person.program) | `timeclock.csv` | `stdout` |
| 112 | [Find purchase-order quantity mismatches](112-reconcile-purchase-quantities.program) | `purchase-lines.csv`, `receipts.csv` | `stdout` |
| 113 | [Find malformed row widths before CSV import](113-audit-unquoted-csv-row-width.program) | `import.csv` | `stdout` |
| 114 | [Normalize phone numbers in a simple contact export](114-normalize-phone-number-punctuation.program) | `phonebook.csv` | `stdout` |
| 115 | [Map vendor category aliases to canonical categories](115-map-vendor-category-aliases.program) | `products-by-alias.csv`, `category-aliases.csv` | `stdout` |
| 116 | [Remove a UTF-8 byte-order mark from export lines](116-remove-utf8-bom-from-export.program) | `export.csv` | `stdout` |
| 117 | [Remove comments and blank rows from a host allowlist](117-clean-commented-host-allowlist.program) | `hosts.txt` | `stdout` |
| 118 | [Normalize NULL and N/A markers in a supplier feed](118-normalize-csv-missing-value-markers.program) | `supplier-feed.csv` | `stdout` |
| 119 | [Build a pick list from ready fulfillment orders](119-build-ready-order-picklist.program) | `fulfillment.csv` | `stdout` |
| 120 | [Flag parcels above standard size or weight limits](120-flag-oversize-parcels.program) | `parcel-manifest.csv` | `stdout` |
| 121 | [Remove query strings from an access export](121-remove-query-strings-from-access-export.program) | `access.tsv` | `stdout` |
| 122 | [Report differences between system and physical stock counts](122-reconcile-cycle-count-variances.program) | `system-counts.csv`, `cycle-counts.csv` | `stdout` |
| 123 | [Profile missing values by CSV column](123-profile-csv-blank-fields.program) | `accounts.csv` | `stdout` |
| 124 | [Total on-hand units by warehouse bin](124-total-units-by-warehouse-bin.program) | `bin-snapshot.csv` | `stdout` |
| 125 | [Trim surrounding spaces from an unquoted CSV description field](125-trim-csv-item-descriptions.program) | `items.csv` | `stdout` |

### 126–150

| ID | Utility | Inputs | Outputs |
| --- | --- | --- | --- |
| 126 | [Stream warning and error records from JSONL](126-jsonl-error-feed.program) | `service.jsonl` | `stdout` |
| 127 | [Watch a three-event deployment verification window](127-tail-deployment-jsonl.program) | `deployments.jsonl`, `stdin` | `stdout` |
| 128 | [Detect out-of-order event timestamps per source](128-service-latency-percentiles.program) | `events.jsonl` | `stdout` |
| 129 | [Allowlist fields for structured log forwarding](129-allowlist-telemetry-fields.program) | `private-events.jsonl` | `stdout` |
| 130 | [Preview event routing decisions](130-event-routing-preview.program) | `routes.json`, `events.jsonl` | `stdout` |
| 131 | [Audit required JSONL event fields](131-jsonl-required-field-audit.program) | `events.jsonl` | `stdout` |
| 132 | [Report requests above a latency objective](132-latency-objective-breach-report.program) | `objective.json`, `requests.jsonl` | `stdout` |
| 133 | [Review latest deployment state per service](133-latest-deployment-state.program) | `deployments.jsonl` | `stdout` |
| 134 | [Export structured audit records to CSV](134-audit-jsonl-to-csv.program) | `audit.jsonl` | `stdout` |
| 135 | [Audit required config setting types](135-config-required-type-audit.program) | `schema.json`, `config.json` | `stdout` |
| 136 | [Coalesce retried webhook deliveries](136-coalesce-webhook-retries.program) | `deliveries.jsonl` | `stdout` |
| 137 | [Group open incidents for an owner handoff](137-incident-owner-handoff.program) | `incidents.jsonl` | `stdout` |
| 138 | [Detect gaps in sequenced event feeds](138-ci-test-failure-digest.program) | `sequenced-events.jsonl` | `stdout` |
| 139 | [Check heartbeat coverage against service inventory](139-service-heartbeat-coverage.program) | `services.json`, `heartbeats.jsonl` | `stdout` |
| 140 | [Format enabled alerts for an operator](140-operator-alert-lines.program) | `alerts.jsonl` | `stdout` |
| 141 | [Normalize a live stdin event feed](141-stdin-event-normalizer.program) | `stdin` | `stdout` |
| 142 | [Gate live events on required fields](142-stdin-event-schema-gate.program) | `stdin` | `stdout` |
| 143 | [Format incidents as they arrive](143-stdin-incident-notifier.program) | `stdin` | `stdout` |
| 144 | [Review current and peak queue depth](144-queue-depth-review.program) | `queues.jsonl` | `stdout` |
| 145 | [Find clients affected by HTTP 503s](145-access-log-503-clients.program) | `access.log` | `stdout` |
| 146 | [Find spans with missing parents in their trace](146-opentelemetry-orphan-parent-audit.program) | `spans.jsonl` | `stdout` |
| 147 | [List owners and versions for failed CSV deployments](147-failed-deployment-owner-list.program) | `deployments.csv` | `stdout` |
| 148 | [Roll up token usage by team and model](148-api-token-usage-rollup.program) | `usage.jsonl` | `stdout` |
| 149 | [Review latest backup status per host](149-backup-status-by-host.program) | `backups.jsonl` | `stdout` |
| 150 | [Build a due-first urgent incident handoff](150-urgent-incident-handoff.program) | `incidents.jsonl` | `stdout` |

## Regression coverage retained from the original ladder

```sh
python3 tests/fixtures/legacy-ladder/run.py --binary bin/execute
python3 tests/utility_catalog.py
```

The nine retained cases cover failed native groups, validation before effects,
record/output limits, cache/checkpoint repair, restored bindings/cwd, and service
recovery. Their expected failures remain failures; they are not presented as
working utilities. The runner boundary tests cover path escape refusal, bounded
capture, preparation without clobbering an input README, and literal `#` parsing.
