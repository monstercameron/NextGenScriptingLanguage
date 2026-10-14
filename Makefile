CARGO ?= cargo
PYTHON ?= python3
NPM ?= npm

.PHONY: check rust-check build proof-smoke

# Keep this sequence shared by local development and macOS/Linux CI.
check: rust-check
	$(PYTHON) tests/model_catalog.py
	$(PYTHON) -O tests/model_catalog.py
	$(PYTHON) tests/unix_bench.py
	$(PYTHON) tests/unix_matrix.py
	$(PYTHON) -O tests/unix_matrix.py
	EXECUTE_PROOF_BINARY="$(CURDIR)/target/release/execute" $(PYTHON) tests/replacement_proofs.py
	EXECUTE_TEST_BINARY="$(CURDIR)/target/release/execute" $(NPM) --prefix editors/vscode test

rust-check:
	$(CARGO) fmt --check
	$(CARGO) clippy --locked --all-targets -- -D warnings
	$(CARGO) test --locked --release -- --test-threads=1
	$(CARGO) build --locked --release

build:
	$(CARGO) build --locked --release

# Functional streaming/lifecycle check, not a timing benchmark or stress proof.
proof-smoke: build
	$(PYTHON) benchmarks/replacement_proofs.py --binary target/release/execute --stream-bytes 1048576 --lifecycle-repetitions 1 --report target/proof-smoke.json
