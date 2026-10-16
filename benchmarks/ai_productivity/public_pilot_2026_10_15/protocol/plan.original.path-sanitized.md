# Small public-development generation pilot

Created: 2026-10-15T23:07:25.379259+00:00

This is a development pilot using the three public task prompts. It is not the held-out 100-script study and supports no economic, superiority, or held-out performance claim.

Repository reference: <FLOW_REPO> at HEAD fa6a2c0fcefc450da28c571127f5372ebaed508c. The checkout was dirty when this plan was recorded; the exact permitted prompt and reference materials are pinned by the SHA-256 values below.

Public task contract SHA-256: e02fc57cb4a40c5ee7f4deb4dc82c80f8a2f649bc614b3adb88295f3ccff1e84

Task prompt hashes (UTF-8 bytes):
- filesystem_inventory: fcb142bbcca01281d98714c8158cba217e7f4b0a86849ec57e6fa9e6db2228e9
- layered_config_merge: 5f5dffef639cafd659e6ca3bfaf020868e22e2fe2d5330180961945cccfd0e8e
- typed_event_normalizer: 596aa3a45dc9c1578478f624fe7ba1643e9d26130a14832fedca9e3ba5695251


Exact participant dispatch prompt hashes (UTF-8 bytes):
- filesystem_inventory__bash: ee379d6fdab648b3d70d9228978538ca97a63456b6d0e278b329912f8c5660ed
- filesystem_inventory__nushell: ffdcacd22b7bc1fc68a5d55668f8b34de267ecd1657f0ec561057484abbad576
- filesystem_inventory__execute: abfb01cf0886d1b7cb166cc1bff69155a62c539d4edc1e8d450c8c8242fb3c41
- layered_config_merge__bash: 0cd4372b27dd702701ccd021095346114d9a0240ab535713766fb16d5e577514
- layered_config_merge__nushell: d15bfcb82aeb7f1cb6ed8d4ffd2d9ea00cc4319bfc50c03c1338f7423b2d9859
- layered_config_merge__execute: 310c19b13e965ad09e90337f2559bc711ca46f0306f9606e1ead25d108af297f
- typed_event_normalizer__bash: 2bd422b620a8c83d16b13613eef3d92690b9da3e6caefaa3fc3fd1268541bc86
- typed_event_normalizer__nushell: 5848a0b86a2a79f13db21d95802cf8cb7d7eb43d8541fcf5c5befe16d7220b9a
- typed_event_normalizer__execute: 2404ef7cbb6c68552ca09aa9e6d07b78870d3445bbcd61d9527257738f889d95

Approved immutable reference copies (each participant receives a private workspace copy):
- docs/language.md: e3adff36821adecc76b67060d2e0c33f894c3fc79370e3d3a2e92f65e66a40de
- manifests/jq.json: 2f6585cff672a91e630833a4b3fc7bc61c9a59d82e5478d926a509d5154f1792
- manifests/sort.json: 6d953c33eed06e702c5781290d76929b8e19d62d283b999321e42de19d779b50
- manifests/ls.json: 3c193cb2c0f1397e6600ddf3de9b8219110a26d5ae5661281c5263f50b74d17d
- manifests/printf.json: abe61c4481c94396e230406897d253ff8ba0f9c854fb09ecc098b85870b0237e
- manifests/cat.json: 483117c32690b423ab88e9443cc7ce0787545f07833576d678b1d0472f19ab3a
- manifests/mkdir.json: 05ff3c97d1b4c03994f3f3d1d3cf0cf868c6b865cf1722501cc240c25a4ec61a
- manifests/core.json_parse.json: 7bf89dc1ae80517ba1a21058dfd9817d69c66be23194779c322b4a616648d05a
- manifests/core.json_stringify.json: 69d121310f54ce696c1bca2d40a1692d0b1a6dbc89928cad95f5218bba5e5585
- manifests/core.json_map.json: d349fc6e8d99190a9df87fda906a9e6c1c3f738fa875d00ef10487f6d8beb119
- manifests/core.json_get.json: 5d0a5618080ae281f5ee84653abb0e045cc5993b132d6400628d2f70910c8537
- manifests/core.json_filter.json: 28a595a004185f48d11df39f8b6a83b713a17897867ecf1e71611531ad9e3cf0
- manifests/core.fs_read.json: 44ed96496c80adc848283d6aee23a3d61535abdfbd4d5f2aa8501058ecca8489
- manifests/core.fs_write_atomic.json: cebaeb1f4088a38a0b8442979cc95f478d2d2a1ef775604f627f7a6799e8506b

All nine participants use model alias gpt-6-luna with reasoning effort max: one Bash, one Nushell, and one Execute generation for each public task. Each prompt contains only its task behavior and input mapping, output serialization rules, and the participant's private workspace. Actual evaluator inputs are supplied later.

The filesystem task writes sorted immediate regular-file names, one per LF-terminated line. The config task emits compact JSON with recursively sorted keys and one final LF. The event task emits compact JSON Lines in input order with keys event_id, occurred_at, kind, source, actor, preserving that order and using null for a missing actor. In every task, stdout and the requested report file must contain the same bytes.

Bash and Nushell arms may read their own candidate and the copied generic manifest references. Execute arms may also read the copied docs/language.md. Participants may not inspect source, examples, tests, benchmark fixtures, oracle data, reference solutions, other candidates, or the internet. Each participant writes only its assigned candidate.sh, candidate.nu, or candidate.program.

This first pass is authoring only. No candidate or Execute binary may be run while root's combined build/check is live. Correctness execution is gated on root authorization against the fresh combined candidate identified in plan.json. No Cargo build, runtime timing benchmark, commit, or push is part of this pilot.

Token use, tool calls, cost, and model inference latency are unavailable. We record participant identity/model/settings, dispatch and return wall-clock times, outcome, exact artifact bytes, and SHA-256. Wall-clock duration is not model inference latency.

Dispatch records and returned artifact hashes are appended to dispatch-ledger.json and results.json after this plan was created. The plan itself was saved before the first dispatch.
