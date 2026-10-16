# Product integration boundary

The combined product integration later landed and was pushed as commit `216a1c848e0305b45971959756d0c5ec3e944929`. Its normal native and WASM hook passed, together with 310 library tests, 20 ignored library tests, 15 graph-runtime tests, 6 lifecycle tests, and 4 result-event tests.

The three benchmark artifacts in this bundle remain tied to their own measured source bases, patches, harnesses, and saved binaries: `81bf9ba766a775cccf059007b19f5def9b9bfb03` for the collector-only wall-clock trial and CPU-counter diagnostic, and `a884f6561fd43f45e43b25a3cc2233be17ee3944` for the isolated `recv_any` trial. Product integration validation does not retarget those measurements, and they do not establish the integrated commit's performance. A performance claim for the final integrated tree would require its own preregistered, source-pinned run.
