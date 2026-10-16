# Small public-development generation pilot

This is a small development pilot using three public task prompts and six public fixture cases. It is not the held-out 100-script study and supports no language-superiority, economic, or held-out-performance claim. First-cohort outcomes retain the original missing Bash filesystem artifact; the later Bash replacement is shown separately.

## Outcomes

Counts are **pass / fail / unavailable** cases.

| Public task | Bash | Nushell | Execute | First-cohort task total |
|---|---:|---:|---:|---:|
| `filesystem_inventory` | 0/0/2 | 0/2/0 | 2/0/0 | 2/2/2 |
| `layered_config_merge` | 2/0/0 | 2/0/0 | 2/0/0 | 6/0/0 |
| `typed_event_normalizer` | 2/0/0 | 0/2/0 | 2/0/0 | 4/2/0 |

| First-cohort language total | Bash | Nushell | Execute | Overall |
|---|---:|---:|---:|---:|
| Pass / fail / unavailable | 4/0/2 | 2/4/0 | 6/0/0 | 12/4/2 |

The first cohort has 18 case rows: **12 pass, 4 fail, 2 unavailable**. The initial `filesystem_inventory__bash` artifact was missing, so its two filesystem cases are unavailable; no substitute was counted in that row. The separately labeled `filesystem_inventory__bash__replacement1` generated artifact passed both filesystem cases, adding 2 pass cases. All rows combined are 20 cases: **14 pass, 4 fail, 2 unavailable**. Exact fixture case IDs and participant-by-case outcomes are in `summary.json` and the compressed evaluator report.

The two `filesystem_inventory__nushell` failures are each one extra LF on stdout; the output file and filesystem delta matched exactly. The two `typed_event_normalizer__nushell` failures emit pretty-printed JSON instead of the required compact, ordered JSON Lines, so stdout and output file content differ. All processes that ran exited 0, with no timeout or runner error. Stderr was captured but is unscored because the public contract does not define an expected stderr stream.

## Method and provenance

All nine initial arms used model alias `gpt-6-luna` with reasoning effort `max`; the separately dispatched replacement used the same settings. Participant dispatch and return observations, original prompt digests, candidate hashes, and the missing-artifact recovery history are in the sanitized dispatch ledgers. They are external wall-clock observations, not inference latency. Cost, token use, tool calls, and inference latency are unavailable. No timed runtime benchmark was performed.

The evaluated Execute source was commit `c1d9ffed3afcdbe37236260a6c0de37807773e9e`, tree `b2004326c008540d4e57880352bfefd1c8409e37`, with Execute binary SHA-256 `f005d5a230705cce75a62e195424018a6ea495ade949ecef993e489f312c0e96`. The manifest catalog digest was `10d776484d2f19a1fb478f9d94882cc056609c7063ef51fd63cb847391b712cc`; the evaluator's before/after Execute and catalog fingerprints matched. Nushell 0.116.0 fingerprint and approved generic docs/manifests are recorded in `protocol/fingerprints.json`. The public task contract and approved generic language references are included. The original plan files remain unchanged in the working campaign and are identified by their exact hashes in the fingerprints file; amendments are included byte-for-byte.

The compressed raw report is lossless for its path-sanitized JSON representation. To avoid publishing local usernames and filesystem layout, three absolute binary/catalog paths became role labels and nine archived-candidate paths became package-relative paths. Case outcomes, exact stdout/stderr byte captures, hashes, and filesystem observations are otherwise unchanged. The original unredacted report remains unchanged in the working campaign and its SHA-256 is recorded in the provenance file.

The generated candidate files in `candidates/` are exact byte copies from the campaign archives, with SHA-256 values in the index. The initially missing Bash file has no candidate copy; the recovery replacement remains its own artifact. No candidate was repaired or re-authored for this package.
