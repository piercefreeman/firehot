# Hot Reload

A POC package to quickly hot reload large Python projects.

```bash
cd mypackage
(cd .. && uv run maturin build)
rm uv.lock
uv sync
uv run python test_hotreload.py
```





















## POC

Eventually the rust logic will be executable in Python. For now you can run the Rust project directly:

```bash
# Build and run the binary with Cargo. We'll want to use `uv run` as a shortcut to run within the virtualenv
# with the third party packages installed in the $PATH
uv run cargo run -- hotreload

# Or build the binary and run it separately
cargo build --release
./target/release/hotreload <path_to_scan>
```


## Installation

### Development Installation

1. Install uv (if not already installed):
   ```bash
   curl -LsSf https://astral.sh/uv/install.sh | sh
   ```

2. Create a virtual environment and install the package in development mode:
   ```bash
   uv sync
   ```

## Usage

After installation, you can use the package:

```python
from hotreload import sum_as_string

result = sum_as_string(5, 7)
print(result)  # Output: "12"
```

## Development

This project uses maturin to build the Rust extension:

```bash
# Build the extension
uv run maturin develop

# Run tests
uv run python test_hotreload.py
```
