# Variables
BINARY := target/release/issun-db
PATH := /snap/bin:$(PATH)
DEBUG_PROJ := 0
RUST_BACKTRACE := 1
ASSET_DIR := docs/assets
SCRIPTS_DIR := scripts
SHELL := /bin/bash
MSRV := 1.85.0

# Binding crates and Python dependency manager
PY_DIR := crates/issundb-py
WHEEL_DIR := dist
PY_MNGR := uv

# Latest built issundb wheel, used by the publish target
WHEEL_FILE := $(shell ls $(PY_DIR)/$(WHEEL_DIR)/issundb-*.whl 2>/dev/null | head -n 1)

# Pinned versions for development tools
LLVM_COV_VERSION := 0.6.16
NEXTEST_VERSION := 0.9.100
AUDIT_VERSION := 0.21.2
CAREFUL_VERSION := 0.4.8

# GraphBLAS initializes a process-global OpenMP pool on first use, and the
# coverage run is process-per-test under nextest, so the pools oversubscribe on
# smaller or loaded machines and a GraphBLAS call can fail intermittently.
# Pinning the pool to one thread and retrying twice compensates. Both are
# overridable from the environment (CI sets them at the job level).
OMP_NUM_THREADS ?= 1
NEXTEST_RETRIES ?= 2

# Default target
.DEFAULT_GOAL := help

.PHONY: help
help: ## Show help messages for all available targets
	@grep -E '^[a-zA-Z0-9_-]+:.*## .*$$' Makefile | \
	awk 'BEGIN {FS = ":.*## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

.PHONY: format
format: ## Format Rust files
	@echo "Formatting Rust files..."
	@cargo fmt

.PHONY: format-check
format-check: ## Check Rust formatting without modifying files (for CI)
	@echo "Checking Rust formatting..."
	@cargo fmt --all --check

.PHONY: doctest
doctest: ## Run documentation tests (code examples in comments)
	@echo "Running documentation tests..."
	@cargo test --doc --workspace

.PHONY: test
test: format doctest ## Run the tests
	@echo "Running tests..."
	@DEBUG_PROJ=$(DEBUG_PROJ) RUST_BACKTRACE=$(RUST_BACKTRACE) cargo test --lib --bins --tests --workspace -- --nocapture

.PHONY: test-conformance
test-conformance: format ## Run the openCypher TCK conformance integration tests
	@echo "Running openCypher TCK conformance integration tests..."
	@DEBUG_PROJ=$(DEBUG_PROJ) RUST_BACKTRACE=$(RUST_BACKTRACE) ISSUNDB_CONFORMANCE=1 cargo test --test conformance -- --nocapture

.PHONY: test-cli
test-cli: format ## Run the CLI integration tests (Unix only)
	@echo "Running CLI integration tests..."
	@./scripts/test_cli.sh

.PHONY: coverage
coverage: format ## Generate test coverage report (llvm-cov over nextest, lcov output)
	@echo "Generating test coverage report..."
	@OMP_NUM_THREADS=$(OMP_NUM_THREADS) NEXTEST_RETRIES=$(NEXTEST_RETRIES) DEBUG_PROJ=$(DEBUG_PROJ) RUST_BACKTRACE=$(RUST_BACKTRACE)\
 	cargo llvm-cov nextest --workspace --exclude issundb-cli\
 	--exclude issundb-py --lcov --output-path lcov.info

.PHONY: build
build: format ## Build the binary for the current platform
	@echo "Building the project..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo build --release

.PHONY: build-zig
build-zig: format ## Build the release binary using `cargo-zigbuild`
	@echo "Building the project with cargo-zigbuild..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo zigbuild --release

.PHONY: build-zig-x64
build-zig-x64: format ## Cross-compile the release binary for x86_64 Linux using `cargo-zigbuild`
	@echo "Cross-compiling for x86_64-unknown-linux-gnu..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo zigbuild --release --target x86_64-unknown-linux-gnu

.PHONY: build-zig-arm64
build-zig-arm64: format ## Cross-compile the release binary for AArch64 Linux using `cargo-zigbuild`
	@echo "Cross-compiling for aarch64-unknown-linux-gnu..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo zigbuild --release --target aarch64-unknown-linux-gnu

.PHONY: run
run: build ## Build and run the binary
	@echo "Running the $(BINARY) binary..."
	@DEBUG_PROJ=$(DEBUG_PROJ) ./$(BINARY)

.PHONY: clean
clean: ## Remove generated and temporary files
	@echo "Cleaning up..."
	@cargo clean
	@rm -rf $(PY_DIR)/$(WHEEL_DIR)
	@rm -f $(PY_DIR)/python/issundb/*.so
	@rm -f $(ASSET_DIR)/*.svg && echo "Removed SVG files; might want to run 'make figs' to regenerate them."

.PHONY: install-snap
install-snap: ## Install a few dependencies using Snapcraft
	@echo "Installing the snap package..."
	@sudo apt-get update
	@# cmake, clang, and libclang build the GraphBLAS submodule (issundb-graphblas-sys);
	@# patchelf lets maturin set the libgomp rpath when packaging the Python wheel.
	@sudo apt-get install -y snapd graphviz wget cmake clang libclang-dev patchelf
	@sudo snap refresh
	@sudo snap install rustup --classic

.PHONY: submodules
submodules: ## Initialize and update all git submodules recursively
	@echo "Initializing and updating all git submodules..."
	@git submodule update --init --recursive

.PHONY: install-deps
install-deps: install-snap submodules ## Install development dependencies
	@echo "Installing dependencies..."
	@rustup component add rustfmt clippy llvm-tools-preview
	@cargo install --locked cargo-llvm-cov --version $(LLVM_COV_VERSION)
	@cargo install --locked cargo-audit --version $(AUDIT_VERSION)
	@cargo install --locked cargo-careful --version $(CAREFUL_VERSION)
	@cargo install --locked cargo-nextest --version $(NEXTEST_VERSION)

.PHONY: install-msrv
install-msrv: ## Install the minimum supported Rust version (MSRV=$(MSRV)) and set it as the default toolchain
	@echo "Installing MSRV toolchain ($(MSRV))..."
	@rustup toolchain install $(MSRV)
	@rustup default $(MSRV)

.PHONY: lint
lint: format ## Run the linters
	@echo "Linting Rust files..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo clippy -- -D warnings -D clippy::unwrap_used -D clippy::expect_used

.PHONY: publish
publish: ## Publish the package to crates.io (requires CARGO_REGISTRY_TOKEN to be set)
	@echo "Publishing the package to Cargo registry..."
	@cargo publish --token $(CARGO_REGISTRY_TOKEN)

########################################################################################
## Python binding targets
########################################################################################

.PHONY: develop-py
develop-py: ## Build issundb-py and install it into the active Python environment
	@echo "Building and installing issundb-py..."
	@# Drop maturin's prior output so a re-run cannot hardlink and re-patch a stale
	@# copy: re-patching zeroes the shared inode (including cargo's target/debug
	@# copy), which cargo's fingerprint then treats as current.
	@rm -rf target/maturin
	@# Maturin fails when CONDA_PREFIX and VIRTUAL_ENV are both set; clear the former.
	@(cd $(PY_DIR) && unset CONDA_PREFIX && maturin develop)

.PHONY: wheel-py
wheel-py: ## Build the issundb-py wheel for the current platform
	@echo "Building the issundb-py wheel..."
	@(cd $(PY_DIR) && maturin build --release --out $(WHEEL_DIR) --auditwheel check)

.PHONY: wheel-py-manylinux
wheel-py-manylinux: ## Build the manylinux issundb-py wheel using Zig
	@echo "Building the manylinux issundb-py wheel..."
	@(cd $(PY_DIR) && maturin build --release --out $(WHEEL_DIR) --auditwheel check --zig)

.PHONY: test-py
test-py: ## Build issundb-py and run the Python binding tests
	@echo "Syncing Python test dependencies..."
	@# Sync the dev group (pytest) without letting uv install the project itself:
	@# uv would register issundb as an editable install, which cannot resolve the
	@# compiled PyO3 extension. Maturin places the working extension instead.
	@(cd $(PY_DIR) && $(PY_MNGR) sync --group dev --no-install-project)
	@echo "Building and installing issundb-py..."
	@# Drop maturin's prior output so it cannot hardlink and re-patch a stale copy:
	@# re-patching an existing artifact zeroes the shared inode (including cargo's
	@# target/debug copy), which cargo's fingerprint then treats as current.
	@rm -rf target/maturin
	@# Maturin fails when CONDA_PREFIX and VIRTUAL_ENV are both set; clear the former.
	@# Run maturin through uv so the dev-group patchelf is on PATH; without it the
	@# libgomp rpath step fails and leaves a corrupt extension.
	@(cd $(PY_DIR) && unset CONDA_PREFIX && $(PY_MNGR) run --no-sync maturin develop)
	@echo "Running Python binding tests..."
	@# --no-sync keeps uv from replacing the maturin-installed extension on run.
	@(cd $(PY_DIR) && $(PY_MNGR) run --no-sync pytest)

.PHONY: publish-py
publish-py: wheel-py-manylinux ## Publish the issundb-py wheel to PyPI (requires PYPI_TOKEN to be set)
	@echo "Publishing issundb-py to PyPI..."
	@if [ -z "$(WHEEL_FILE)" ]; then \
	   echo "Error: no wheel file found in $(PY_DIR)/$(WHEEL_DIR). Run 'make wheel-py-manylinux' first."; \
	   exit 1; \
	fi
	@echo "Found wheel file: $(WHEEL_FILE)"
	@twine upload -u __token__ -p $(PYPI_TOKEN) $(WHEEL_FILE)

.PHONY: repl
repl: ## Launch the REPL (pass REPL_PATH=<dir> to set the database path; defaults to ./issundb-data)
	@echo "Starting IssunDB REPL (database: $(or $(REPL_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-cli -- $(or $(REPL_PATH),./issundb-data)


.PHONY: mcp
mcp: ## Launch the MCP server over stdio (pass MCP_PATH=<dir> to set the database path; defaults to ./issundb-data)
	@echo "Starting IssunDB MCP server (database: $(or $(MCP_PATH),./issundb-data))..." >&2
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -q -p issundb-mcp -- --db-path $(or $(MCP_PATH),./issundb-data)

.PHONY: mcp-http
mcp-http: ## Launch the MCP server over Streamable HTTP (MCP_PATH=<dir> db path, MCP_BIND=<addr> bind address)
	@echo "Starting IssunDB MCP server over HTTP at $(or $(MCP_BIND),127.0.0.1:8000) (database: $(or $(MCP_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-mcp -- --db-path $(or $(MCP_PATH),./issundb-data)\
 	--transport http --bind $(or $(MCP_BIND),127.0.0.1:8000)

.PHONY: rest
rest: ## Launch the HTTP REST API server (pass REST_PATH=<dir> db path, REST_HOST=<addr> bind host, REST_PORT=<port> port)
	@echo "Starting IssunDB REST API server at $(or $(REST_HOST),127.0.0.1):$(or $(REST_PORT),7474) (database: $(or $(REST_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-rest -- --db-path $(or $(REST_PATH),./issundb-data)\
 	--host $(or $(REST_HOST),127.0.0.1) --port $(or $(REST_PORT),7474)

.PHONY: bench
bench: ## Run all workspace benchmarks
	@echo "Running all benchmarks..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench

.PHONY: bench-large
bench-large: ## Run all benchmarks including large storage and load tests
	@echo "Running all benchmarks including large storage and load tests..."
	@DEBUG_PROJ=$(DEBUG_PROJ) ISSUNDB_LARGE_BENCH=1 ISSUNDB_LOAD_TEST=1 cargo bench

.PHONY: bench-storage
bench-storage: ## Run core storage engine and analytics benchmarks
	@echo "Running core storage benchmarks..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench -p issundb-core

.PHONY: bench-vector
bench-vector: ## Run vector search benchmarks
	@echo "Running vector search benchmarks..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench -p issundb-vector

.PHONY: bench-text
bench-text: ## Run full-text search benchmarks
	@echo "Running full-text search benchmarks..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench -p issundb-text

.PHONY: bench-retrieval
bench-retrieval: ## Run hybrid retrieval benchmarks
	@echo "Running hybrid retrieval benchmarks..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench -p issundb-retrieval

.PHONY: bench-cypher
bench-cypher: ## Run Cypher parser and query optimizer benchmarks
	@echo "Running Cypher and query optimizer benchmarks..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench -p issundb-cypher
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench -p issundb --bench query_optimizer

.PHONY: bench-ladybugdb
bench-ladybugdb: ## Run the LadybugDB comparison harness
	@echo "Running LadybugDB comparison harness..."
	@cd benchmarks/ladybugdb-compare && cargo run --release

.PHONY: audit
audit: ## Run security audit on Rust dependencies
	@echo "Running security audit..."
	@cargo audit

.PHONY: deny
deny: ## Check dependencies for advisories, license compliance, and duplicates
	@echo "Running cargo-deny..."
	@cargo deny check

.PHONY: careful
careful: ## Run tests under cargo-careful (detects undefined behavior and unsafe misuse)
	@echo "Running tests under cargo-careful..."
	@DEBUG_PROJ=$(DEBUG_PROJ) RUST_BACKTRACE=$(RUST_BACKTRACE) cargo careful test --lib --bins --tests --workspace

.PHONY: docs
docs: format ## Generate the documentation
	@echo "Generating Rust API documentation..."
	@cargo doc --no-deps --document-private-items
	@echo "Generating MkDocs documentation..."
	@uv run python -c "import yaml.nodes; import yaml; yaml.Node = yaml.nodes.Node; from mkdocs.__main__ import cli; cli()" build

.PHONE: figs
figs: ## Generate the figures in the assets directory
	@echo "Generating figures..."
	@$(SHELL) $(ASSET_DIR)/diagrams/make_figures.sh $(ASSET_DIR)/diagrams

.PHONY: fix-lint
fix-lint: ## Fix the linter warnings
	@echo "Fixing linter warnings..."
	@cargo clippy --fix --allow-dirty --allow-staged --all-targets --workspace --all-features -- -D warnings\
 	-D clippy::unwrap_used -D clippy::expect_used

.PHONY: run-examples
run-examples: ## Run all examples in crates/issundb-examples one by one
	@echo "Running all examples..."
	@for example in crates/issundb-examples/*.rs; do \
	   example_name=$$(basename $$example .rs); \
	   echo "Running example: $$example_name"; \
	   cargo run -p issundb-examples --example $$example_name; \
	done

.PHONY: check-module-deps
check-module-deps: ## Verify crate boundary rules: lower-level crates must not import from higher-level crates
	@echo "Checking crate dependency boundaries..."
	@ERROR=0; \
	declare -A FORBIDDEN; \
	FORBIDDEN[issundb-core]="issundb_vector issundb_text issundb_retrieval issundb_cypher issundb_cli"; \
	FORBIDDEN[issundb-vector]="issundb_text issundb_retrieval issundb_cypher issundb_cli"; \
	FORBIDDEN[issundb-text]="issundb_vector issundb_retrieval issundb_cypher issundb_cli"; \
	FORBIDDEN[issundb-retrieval]="issundb_cypher issundb_cli"; \
	FORBIDDEN[issundb-cypher]="issundb_cli"; \
	for crate in issundb-core issundb-vector issundb-text issundb-retrieval issundb-cypher; do \
	   src_dir="crates/$$crate/src"; \
	   if [ ! -d "$$src_dir" ]; then continue; fi; \
	   for forbidden in $${FORBIDDEN[$$crate]}; do \
	      VIOLATIONS=$$(grep -r "use $$forbidden" "$$src_dir/" 2>/dev/null || true); \
	      if [ -n "$$VIOLATIONS" ]; then \
	         echo "ERROR: $$crate has forbidden dependency on $$forbidden:"; \
	         echo "$$VIOLATIONS" | sed 's/^/  /'; \
	         ERROR=1; \
	      fi; \
	   done; \
	done; \
	if [ $$ERROR -eq 0 ]; then \
	   echo "All crate dependency boundaries are valid."; \
	else \
	   echo "Crate boundary violations found. See AGENTS.md for the dependency rules."; \
	   exit 1; \
	fi

.PHONY: testdata
testdata: ## Regenerate versioned LMDB snapshots
	@echo "Regenerating versioned test snapshots..."
	@VERSION=$$(cargo metadata --no-deps --format-version 1 | python3 -c "import sys,json; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name'] == 'issundb'))"); \
	 SNAP_DIR="test_data/v$$VERSION/db"; \
	 mkdir -p "$$SNAP_DIR"; \
	 cargo run -p issundb-core --bin gen_testdata -- "$$SNAP_DIR"
	@echo "Snapshot written. Commit test_data/ to record the current storage format."

.PHONY: oracle-fixtures
oracle-fixtures: ## Regenerate the NetworkX oracle corpora (needs Python3 and NetworkX)
	@echo "Regenerating NetworkX oracle corpora..."
	@python3 $SCRIPTS_DIR/gen_oracle_fixtures.py crates/issundb/tests/fixtures/networkx_oracle.json
	@python3 $SCRIPTS_DIR/gen_pagerank_fixtures.py crates/issundb/tests/fixtures/networkx_pagerank.json
	@python3 $SCRIPTS_DIR/gen_centrality_fixtures.py crates/issundb/tests/fixtures/networkx_centrality.json
	@python3 $SCRIPTS_DIR/gen_paths_fixtures.py crates/issundb/tests/fixtures/networkx_paths.json
	@echo "Corpora written. Commit crates/issundb/tests/fixtures/ to record the oracle."

.PHONY: nextest
nextest: ## Run tests using nextest
	@echo "Running tests using nextest..."
	@OMP_NUM_THREADS=$(OMP_NUM_THREADS) NEXTEST_RETRIES=$(NEXTEST_RETRIES) DEBUG_PROJ=$(DEBUG_PROJ)\
 	RUST_BACKTRACE=$(RUST_BACKTRACE) cargo nextest run

.PHONY: setup-hooks
setup-hooks: ## Install Git hooks (pre-commit and pre-push)
	@echo "Setting up Git hooks..."
	@if ! command -v pre-commit &> /dev/null; then \
	   echo "pre-commit not found. Please install it using 'pip install pre-commit'"; \
	   exit 1; \
	fi
	@pre-commit install --hook-type pre-commit
	@pre-commit install --hook-type pre-push
	@pre-commit install-hooks

.PHONY: test-hooks
test-hooks: ## Test Git hooks on all files
	@echo "Testing Git hooks..."
	@pre-commit run --all-files
