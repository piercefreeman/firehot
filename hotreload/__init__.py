"""
Hot Reload - A Python package with Rust extensions.

This package provides tools for isolating imports and executing code in forked processes
to avoid reloading the entire application during development.
"""
from hotreload.hotreload import isolate_imports as isolate_imports_rs
from contextlib import contextmanager

@contextmanager
def isolate_imports(package_path: str):
    """
    Isolate imports for the given package path.
    """
    try:
        isolate_imports_rs(package_path)
        yield
    finally:
        stop_import_runner_rs()
