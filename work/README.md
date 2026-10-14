# Retained optimization preimages

`optimization-8h/` contains historical patch preimages and a retained streaming
regression source from the earlier optimization campaign. They are evidence,
not current implementation files or an active task queue. Current code and tests
live in `src/` and `tests/`.

Keep these files unchanged when doing routine cleanup. New build output belongs
in ignored `target/`; campaign scratch belongs outside the repository. See the
[benchmark archive](../benchmarks/optimization-pass/README.md) and
[evidence index](../docs/README.md#historical-evidence).
