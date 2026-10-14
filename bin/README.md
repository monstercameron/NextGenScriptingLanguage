# Historical binary

`execute` in this directory is the retained **0.8.0 macOS arm64** artifact. It is
kept unchanged for historical validation and compatibility comparisons.

For current features, use a [release download](../docs/releases.md) or run
`cargo build --locked --release` and use `target/release/execute`. Release
workflows compile fresh executables; they do not package this historical file.
