# Variables
BINARY := target/release/issun-db
PATH := /snap/bin:$(PATH)
DEBUG_PROJ := 0
RUST_BACKTRACE := 1
ASSET_DIR := assets
SHELL := /bin/bash
MSRV := 1.85.0

# Binding crates and Python dependency manager
PY_DIR := crates/issundb-py
NODE_DIR := crates/issundb-node
WHEEL_DIR := dist
PY_MNGR := uv

# Latest built issundb wheel, used by the publish target
WHEEL_FILE := $(shell ls $(PY_DIR)/$(WHEEL_DIR)/issundb-*.whl 2>/dev/null | head -n 1)

# Pinned versions for development tools
TARPAULIN_VERSION := 0.32.8
NEXTEST_VERSION := 0.9.101
AUDIT_VERSION := 0.21.2
CAREFUL_VERSION := 0.4.8

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

.PHONY: coverage
coverage: format ## Generate test coverage report
	@echo "Generating test coverage report..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo tarpaulin --out Xml --out Html

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
	@rm -f $(NODE_DIR)/*.node
	@rm -f $(ASSET_DIR)/*.svg && echo "Removed SVG files; might want to run 'make figs' to regenerate them."

.PHONY: install-snap
install-snap: ## Install a few dependencies using Snapcraft
	@echo "Installing the snap package..."
	@sudo apt-get update
	@sudo apt-get install -y snapd graphviz wget
	@sudo snap refresh
	@sudo snap install rustup --classic

.PHONY: submodules
submodules: ## Initialize and update all git submodules recursively
	@echo "Initializing and updating all git submodules..."
	@git submodule update --init --recursive

.PHONY: install-deps
install-deps: install-snap submodules ## Install development dependencies
	@echo "Installing dependencies..."
	@rustup component add rustfmt clippy
	@cargo install --locked cargo-tarpaulin --version $(TARPAULIN_VERSION)
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
## Python and Node.js binding targets
########################################################################################

.PHONY: develop-py
develop-py: ## Build issundb-py and install it into the active Python environment
	@echo "Building and installing issundb-py..."
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
test-py: develop-py ## Build issundb-py and run the Python binding tests
	@echo "Running Python binding tests..."
	@(cd $(PY_DIR) && $(PY_MNGR) run pytest)

.PHONY: publish-py
publish-py: wheel-py-manylinux ## Publish the issundb-py wheel to PyPI (requires PYPI_TOKEN to be set)
	@echo "Publishing issundb-py to PyPI..."
	@if [ -z "$(WHEEL_FILE)" ]; then \
	   echo "Error: no wheel file found in $(PY_DIR)/$(WHEEL_DIR). Run 'make wheel-py-manylinux' first."; \
	   exit 1; \
	fi
	@echo "Found wheel file: $(WHEEL_FILE)"
	@twine upload -u __token__ -p $(PYPI_TOKEN) $(WHEEL_FILE)

.PHONY: build-node
build-node: ## Build the issundb-node native addon for the current platform
	@echo "Building the issundb-node native addon..."
	@(cd $(NODE_DIR) && npm install && npm run build)

.PHONY: test-node
test-node: build-node ## Build issundb-node and run the Node.js binding tests
	@echo "Running Node.js binding tests..."
	@(cd $(NODE_DIR) && npm test)

.PHONY: repl
repl: ## Launch the REPL (pass REPL_PATH=<dir> to set the database path; defaults to ./issundb-data)
	@echo "Starting IssunDB REPL (database: $(or $(REPL_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-cli -- $(or $(REPL_PATH),./issundb-data)

.PHONY: gui
gui: ## Launch the GUI (pass GUI_PATH=<dir> to set the database path; defaults to ./issundb-data)
	@echo "Starting IssunDB GUI (database: $(or $(GUI_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-gui -- $(or $(GUI_PATH),./issundb-data)

.PHONY: mcp
mcp: ## Launch the MCP server over stdio (pass MCP_PATH=<dir> to set the database path; defaults to ./issundb-data)
	@echo "Starting IssunDB MCP server (database: $(or $(MCP_PATH),./issundb-data))..." >&2
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -q -p issundb-mcp -- --db-path $(or $(MCP_PATH),./issundb-data)

.PHONY: mcp-http
mcp-http: ## Launch the MCP server over Streamable HTTP (MCP_PATH=<dir> db path, MCP_BIND=<addr> bind address)
	@echo "Starting IssunDB MCP server over HTTP at $(or $(MCP_BIND),127.0.0.1:8000) (database: $(or $(MCP_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-mcp -- --db-path $(or $(MCP_PATH),./issundb-data) --transport http --bind $(or $(MCP_BIND),127.0.0.1:8000)

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
	@$(SHELL) $(ASSET_DIR)/make_figures.sh $(ASSET_DIR)

.PHONY: fix-lint
fix-lint: ## Fix the linter warnings
	@echo "Fixing linter warnings..."
	@cargo clippy --fix --allow-dirty --allow-staged --all-targets --workspace --all-features -- -D warnings -D clippy::unwrap_used -D clippy::expect_used

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
	@VERSION=$$(cargo metadata --no-deps --format-version 1 | python3 -c "import sys,json; print(json.load(sys.stdin)['packages'][0]['version'])"); \
	 SNAP_DIR="test_data/v$$VERSION/db"; \
	 mkdir -p "$$SNAP_DIR"; \
	 cargo run -p issundb-testing --bin gen_testdata -- "$$SNAP_DIR"
	@echo "Snapshot written. Commit test_data/ to record the current storage format."

.PHONY: nextest
nextest: ## Run tests using nextest
	@echo "Running tests using nextest..."
	@DEBUG_PROJ=$(DEBUG_PROJ) RUST_BACKTRACE=$(RUST_BACKTRACE) cargo nextest run

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
	@pre-commit run --all-files --show-diff-on-failure
