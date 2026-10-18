# GitHub Pages showcase

The landing page lives in `docs/index.html` and `docs/assets/`. The Rust/WASM
playground uses tracked HTML in `docs/playground/` and generated assets built from
`browser/` and `web/`. `.github/workflows/pages.yml` builds and tests the playground,
then deploys the complete `docs/` artifact through GitHub Pages. `.nojekyll` keeps
all files available without Jekyll processing.

The landing page covers the positioning (an AI-first Unix scripting language that people
can read and observe), the curated scripts, the thirty playground workflows (links use
`playground/index.html#example=ID`), the terminal captures, the benchmark evidence, AI
applets and the build steps. The comparison table in the numbers section is static HTML
copied from the Linux x86_64 run of `tools/bash_compare.py` (commit `234c1fc`) and the
README's performance section. When a newer run changes those numbers, edit the table, the
"2 of 8" count and the surrounding sentences together, and keep the statement that
optimisation is ongoing and that Execute loses startup, size and memory. The earlier
paired macOS chart below it is drawn from the raw JSON reports in `assets/data`.

The landing page itself is static and buildless. Build the playground before
previewing the whole site:

```sh
make setup
rustup target add wasm32-unknown-unknown
make playground-build
```

Preview from the repository root:

```sh
python3 -m http.server 8765 --bind 127.0.0.1 --directory docs
```

Open `http://127.0.0.1:8765/`. The page uses relative asset URLs so project Pages
subpaths work. Documentation links go to GitHub's Markdown reader; raw JSON
receipts stay available on the site. Google Fonts are optional presentation
resources with local system fallbacks. The site has no analytics or model calls.

## Keep the examples and evidence real

The curated `assets/data/examples.json` includes the canonical repository path,
source text, download path and source SHA-256 for every example. Edit its display
metadata to curate the collection, then refresh source/download/benchmark copies:

```sh
python3 tools/sync_site.py
python3 tests/site.py
node --check docs/assets/site.js
```

`make check` also validates all local links, fragment targets, exact canonical
script copies, actual `.program` validation, the hero's real two-line-count
output, benchmark sample counts/copies and screenshot source provenance. It
never executes the displayed AI or desktop applications.

The earlier-measurement selector uses two exact 80-sample paired reports. The page reports
helper-process avoidance, keeps Bash's faster result visible, and links the raw
samples. Those measurements describe recorded 0.9.0-dev source hashes before
the Windows port. The page identifies the 0.9.0-rc.4 release, keeps the retained
0.8.0 binary distinct, and links unfinished production gates.

## Screenshots

`assets/captures/` contains two actual PTY frames from the source runtime,
rendered as PNG from captured ANSI cells. The original `.program` files are
unchanged. Only their native `ps` and `gh` backends use deterministic fixture
metrics for the captures. This avoids displaying local process data or presenting
sample star counts as real GitHub observations. The captions and provenance JSON
state this explicitly. The line graph is the real widget; Braille cells are
rendered from their eight-dot Unicode bit patterns when Menlo lacks the glyph.

To regenerate on macOS, use a Python environment containing `pyte` and `Pillow`,
run from the repository root after building the source:

```sh
cargo build --locked --release
python3 tools/capture_site.py
```

The capture helper uses the system Menlo font and private temporary fixture
backends. It never queries a provider, GitHub, the package manager or a desktop
service. It exits its owned TUI instances and removes temporary fixtures.
Python image/capture packages are documentation tooling, not runtime dependencies.

## Browser checks

The first site pass was checked in a real browser at desktop and mobile
breakpoints. Example selection/filtering, the benchmark repeat selector, loaded
screenshots, clipboard copy (with original contents restored), local links,
source validation and a mobile overflow check passed. Reduced-motion styles,
keyboard focus, descriptive images, skip navigation and readable chart values
are included. No automated accessibility certification or performance score is
claimed.
