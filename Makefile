.PHONY: lint lint-ruff lint-pyright lint-hotreload lint-mypackage lint-external

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
	(cd $(1) && uv run pyright) || { echo "FAILED: pyright in $(1)"; exit 1; }; \
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

# Show help
help:
	@echo "Available targets:"
	@echo "  lint            - Run all linters on all packages"
	@echo "  lint-hotreload  - Run all linters on the root package only"
	@echo "  lint-mypackage  - Run all linters on mypackage only"
	@echo "  lint-external   - Run all linters on external-package only"
	@echo "  lint-ruff       - Run ruff linter only (all packages)"
	@echo "  lint-pyright    - Run pyright type checker only (all packages)"
