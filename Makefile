# Variables
BINARY := target/release/issun-db
PATH := /snap/bin:$(PATH)
DEBUG_PROJ := 0
RUST_BACKTRACE := 1
ASSET_DIR := assets
TEST_DATA_DIR := tests/testdata
SHELL := /bin/bash
MSRV := 1.85.0

# Pinned versions for development tools
TARPAULIN_VERSION := 0.32.8
NEXTEST_VERSION := 0.9.101
AUDIT_VERSION := 0.21.2
CAREFUL_VERSION := 0.4.8

# Default target
.DEFAULT_GOAL := help

.PHONY: help
help: ## Show help messages for all available targets
	@grep -E '^[a-zA-Z_-]+:.*## .*$$' Makefile | \
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
	@DEBUG_PROJ=$(DEBUG_PROJ) RUST_BACKTRACE=$(RUST_BACKTRACE) cargo test --all-targets --workspace -- --nocapture

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

.PHONY: run
run: build ## Build and run the binary
	@echo "Running the $(BINARY) binary..."
	@DEBUG_PROJ=$(DEBUG_PROJ) ./$(BINARY)

.PHONY: clean
clean: ## Remove generated and temporary files
	@echo "Cleaning up..."
	@cargo clean
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

.PHONY: repl
repl: ## Launch the interactive REPL (pass REPL_PATH=<dir> to set the database path; defaults to ./issundb-data)
	@echo "Starting IssunDB REPL (database: $(or $(REPL_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-cli -- $(or $(REPL_PATH),./issundb-data)

.PHONY: gui
gui: ## Launch the graphical desktop user interface (pass GUI_PATH=<dir> to set the database path; defaults to ./issundb-data)
	@echo "Starting IssunDB GUI (database: $(or $(GUI_PATH),./issundb-data))..."
	@RUST_BACKTRACE=$(RUST_BACKTRACE) cargo run -p issundb-gui -- $(or $(GUI_PATH),./issundb-data)

.PHONY: bench
bench: ## Run the benchmarks
	@echo "Running benchmarks..."
	@DEBUG_PROJ=$(DEBUG_PROJ) cargo bench

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
	@DEBUG_PROJ=$(DEBUG_PROJ) RUST_BACKTRACE=$(RUST_BACKTRACE) cargo careful test --all-targets --workspace

.PHONY: docs
docs: format ## Generate the documentation
	@echo "Generating documentation..."
	@cargo doc --no-deps --document-private-items

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
testdata: ## Download benchmark datasets and regenerate versioned LMDB snapshots
	@echo "Downloading benchmark datasets..."
	@$(SHELL) $(TEST_DATA_DIR)/download_datasets.sh $(TEST_DATA_DIR)
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
