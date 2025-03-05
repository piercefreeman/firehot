"""
Hot Reload - A Python package with Rust extensions.

This package provides tools for isolating imports and executing code in forked processes
to avoid reloading the entire application during development.
"""
from hotreload.hotreload import start_import_runner as start_import_runner_rs, stop_import_runner as stop_import_runner_rs
from contextlib import contextmanager

@contextmanager
def isolate_imports(package_path: str):
    """
    Isolate imports for the given package path.
    """
    try:
        payload = start_import_runner_rs(package_path)
        print("Payload", payload)
        yield
    finally:
        #stop_import_runner_rs()
        pass
