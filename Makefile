CARGO ?= cargo
RUSTFMT ?= rustfmt
PYTHON ?= python3
NPM ?= npm
PORT ?= 8080

.DEFAULT_GOAL := help
.PHONY: help setup format format-check docs-check check rust-check build proof-smoke unix-smoke playground-build playground-check playground-browser-check site-serve

help:
	@printf '%s\n' \
	  'Execute contributor commands' \
	  '  make setup                    Install locked editor and playground npm dependencies' \
	  '  make build                    Build native release executables' \
	  '  make check                    Native Rust, documentation, Python and editor checks' \
	  '  make rust-check               Native formatting, Clippy and release tests' \
	  '  make docs-check               Check maintained local documentation links' \
	  '  make format                   Format owned Rust and playground source' \
	  '  make format-check             Check formatting without modifying files' \
	  '  make playground-build         Build native CLI and Rust/WASM playground assets' \
	  '  make playground-check         WASM lint/build, formatting and runtime contracts' \
	  '  make playground-browser-check Run real Chromium acceptance after contracts' \
	  '  make site-serve               Build and preview Pages on 127.0.0.1:$(PORT)' \
	  '  make unix-smoke               Native command comparison smoke report' \
	  '  make proof-smoke              Bounded streaming and lifecycle smoke report' \
	  'See CONTRIBUTING.md for prerequisites and browser installation.'

setup:
	$(NPM) --prefix editors/vscode ci --ignore-scripts --no-audit --no-fund
	$(NPM) --prefix web ci --ignore-scripts --no-audit --no-fund

format:
	$(CARGO) fmt
	$(CARGO) fmt --manifest-path browser/Cargo.toml
	$(RUSTFMT) --edition 2021 src/runtime/shared.rs
	$(NPM) --prefix web run format

format-check:
	$(CARGO) fmt --check
	$(CARGO) fmt --manifest-path browser/Cargo.toml --check
	$(RUSTFMT) --edition 2021 --check src/runtime/shared.rs
	$(NPM) --prefix web run format:check

docs-check:
	$(PYTHON) tests/docs.py
	$(PYTHON) tools/check_docs.py

# Keep this sequence shared by local development and macOS/Linux CI.
check: rust-check docs-check
	$(PYTHON) tests/releases.py
	$(PYTHON) -O tests/releases.py
	$(PYTHON) tests/model_catalog.py
	$(PYTHON) -O tests/model_catalog.py
	EXECUTE_TEST_BINARY="$(CURDIR)/target/release/execute" $(PYTHON) tests/omarchy_interfaces.py
	EXECUTE_TEST_BINARY="$(CURDIR)/target/release/execute" $(PYTHON) tests/site.py
	$(PYTHON) tests/unix_bench.py
	$(PYTHON) tests/unix_matrix.py
	$(PYTHON) -O tests/unix_matrix.py
	EXECUTE_PROOF_BINARY="$(CURDIR)/target/release/execute" $(PYTHON) tests/replacement_proofs.py
	EXECUTE_TEST_BINARY="$(CURDIR)/target/release/execute" $(NPM) --prefix editors/vscode test

rust-check:
	$(CARGO) fmt --check
	$(RUSTFMT) --edition 2021 --check src/runtime/shared.rs
	$(CARGO) clippy --locked --all-targets -- -D warnings
	$(CARGO) test --locked --release -- --test-threads=1
	$(CARGO) build --locked --release

build:
	$(CARGO) build --locked --release

playground-build: build
	CARGO="$(CARGO)" NPM="$(NPM)" $(PYTHON) tools/build_playground.py

playground-check: playground-build
	$(CARGO) fmt --manifest-path browser/Cargo.toml --check
	$(CARGO) clippy --manifest-path browser/Cargo.toml --locked --target wasm32-unknown-unknown -- -D warnings
	$(NPM) --prefix web run format:check
	$(NPM) --prefix web test

# Install Chromium once with: npm --prefix web exec -- playwright install chromium
playground-browser-check: playground-check
	$(NPM) --prefix web run test:browser

site-serve: playground-build
	$(PYTHON) -m http.server $(PORT) --bind 127.0.0.1 --directory docs

# Functional streaming/lifecycle check, not a timing benchmark or stress proof.
proof-smoke: build
	$(PYTHON) benchmarks/replacement_proofs.py --binary target/release/execute --stream-bytes 1048576 --lifecycle-repetitions 1 --native-failure-code 7 --report target/proof-smoke.json

# Native command contracts on the runner host, not Arch/Omarchy desktop acceptance.
unix-smoke: build
	$(PYTHON) benchmarks/unix_matrix.py --binary target/release/execute --samples 1 --warmups 0 --report target/unix-matrix.json
