.PHONY: lint lint-ruff lint-pyright lint-hotreload lint-mypackage lint-external ci-lint ci-lint-ruff ci-lint-pyright ci-lint-hotreload ci-lint-mypackage ci-lint-external

# Default target
all: lint

# Package directories
ROOT_DIR := ./hotreload/
MYPACKAGE_DIR := ./mypackage/mypackage/
EXTERNAL_DIR := ./mypackage/external-package/
PKG_DIRS := $(ROOT_DIR) $(MYPACKAGE_DIR) $(EXTERNAL_DIR)

# Define a function to run pyright on a specific directory
# Usage: $(call run_pyright,<directory>)
define run_pyright
	@echo "\n=== Running pyright on $(1) ==="; \
	(cd $(1) && echo '{"include": ["."], "exclude": [".."], "ignore": ["../"]}' > temp_pyright_config.json && \
	uv run pyright --project temp_pyright_config.json && \
	rm temp_pyright_config.json) || { echo "FAILED: pyright in $(1)"; exit 1; }; \
	echo "=== pyright completed successfully for $(1) ===";
endef

# Define a function to run ruff on a specific directory 
# Usage: $(call run_ruff,<directory>)
define run_ruff
	@echo "\n=== Running ruff on $(1) ==="; \
	echo "Running ruff format in $(1)"; \
	(cd $(1) && uv run ruff format .) || { echo "FAILED: ruff format in $(1)"; exit 1; }; \
	echo "Running ruff check --fix in $(1)"; \
	(cd $(1) && uv run ruff check --fix .) || { echo "FAILED: ruff check in $(1)"; exit 1; }; \
	echo "=== ruff completed successfully for $(1) ===";
endef

# Define a function to run all lint tools on a specific directory
# Usage: $(call lint_directory,<directory>)
define lint_directory
	@echo "\n=== Running all linters on $(1) ===";
	$(call run_ruff,$(1))
	$(call run_pyright,$(1))
	@echo "=== All linters completed successfully for $(1) ===";
endef

# Main lint target that runs all linting tools on all packages
lint: lint-hotreload lint-mypackage lint-external

# Package-specific lint targets
lint-hotreload:
	@echo "=== Linting hotreload package ==="
	$(call lint_directory,$(ROOT_DIR))

lint-mypackage:
	@echo "=== Linting mypackage package ==="
	$(call lint_directory,$(MYPACKAGE_DIR))

lint-external:
	@echo "=== Linting external package ==="
	$(call lint_directory,$(EXTERNAL_DIR))

# Tool-specific targets that run across all packages
lint-ruff:
	@echo "=== Running ruff on all packages ==="
	@for dir in $(PKG_DIRS); do \
		$(call run_ruff,$$dir); \
	done
	@echo "\n=== Ruff linting completed successfully for all packages ==="

lint-pyright:
	@echo "=== Running pyright on all packages ==="
	@for dir in $(PKG_DIRS); do \
		$(call run_pyright,$$dir); \
	done
	@echo "\n=== Pyright type checking completed successfully for all packages ==="

# Define a function to run ruff in CI mode (check only, no fixes)
# Usage: $(call run_ruff_ci,<directory>)
define run_ruff_ci
	@echo "\n=== Running ruff (validation only) on $(1) ==="; \
	echo "Running ruff format --check in $(1)"; \
	(cd $(1) && uv run ruff format --check .) || { echo "FAILED: ruff format in $(1)"; exit 1; }; \
	echo "Running ruff check (no fix) in $(1)"; \
	(cd $(1) && uv run ruff check .) || { echo "FAILED: ruff check in $(1)"; exit 1; }; \
	echo "=== ruff validation completed successfully for $(1) ===";
endef

# Define a function to run all lint tools in CI mode on a specific directory
# Usage: $(call lint_directory_ci,<directory>)
define lint_directory_ci
	@echo "\n=== Running all linters (validation only) on $(1) ===";
	$(call run_ruff_ci,$(1))
	$(call run_pyright,$(1))
	@echo "=== All linters completed successfully for $(1) ===";
endef

# CI lint target that runs all linting tools on all packages (no fixes)
ci-lint: ci-lint-hotreload ci-lint-mypackage ci-lint-external

# Package-specific CI lint targets (no fixes)
ci-lint-hotreload:
	@echo "=== CI Linting hotreload package (validation only) ==="
	$(call lint_directory_ci,$(ROOT_DIR))

ci-lint-mypackage:
	@echo "=== CI Linting mypackage package (validation only) ==="
	$(call lint_directory_ci,$(MYPACKAGE_DIR))

ci-lint-external:
	@echo "=== CI Linting external package (validation only) ==="
	$(call lint_directory_ci,$(EXTERNAL_DIR))

# Tool-specific CI targets that run across all packages (no fixes)
ci-lint-ruff:
	@echo "=== Running ruff (validation only) on all packages ==="
	@for dir in $(PKG_DIRS); do \
		$(call run_ruff_ci,$$dir); \
	done
	@echo "\n=== Ruff validation completed successfully for all packages ==="

ci-lint-pyright:
	@echo "=== Running pyright on all packages ==="
	@for dir in $(PKG_DIRS); do \
		$(call run_pyright,$$dir); \
	done
	@echo "\n=== Pyright type checking completed successfully for all packages ==="

# Show help
help:
	@echo "Available targets:"
	@echo " "
	@echo "  lint            - Run all linters on all packages (with fixes)"
	@echo "  lint-hotreload  - Run all linters on the root package only (with fixes)"
	@echo "  lint-mypackage  - Run all linters on mypackage only (with fixes)"
	@echo "  lint-external   - Run all linters on external-package only (with fixes)"
	@echo "  lint-ruff       - Run ruff linter only (all packages, with fixes)"
	@echo "  lint-pyright    - Run pyright type checker only (all packages)"
	@echo " "
	@echo "  ci-lint         - Run all linters on all packages (validation only, no fixes)"
	@echo "  ci-lint-hotreload - Run all linters on the root package only (validation only)"
	@echo "  ci-lint-mypackage - Run all linters on mypackage only (validation only)"
	@echo "  ci-lint-external - Run all linters on external-package only (validation only)"
	@echo "  ci-lint-ruff    - Run ruff linter only (all packages, validation only)"
	@echo "  ci-lint-pyright - Run pyright type checker only (all packages)"
