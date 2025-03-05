"""
Hot Reload - A Python package with Rust extensions.

This package provides tools for isolating imports and executing code in forked processes
to avoid reloading the entire application during development.
"""
from hotreload.hotreload import start_import_runner as start_import_runner_rs, stop_import_runner as stop_import_runner_rs, exec_isolated as exec_isolated_rs, communicate_isolated as communicate_isolated_rs
from contextlib import contextmanager
from typing import Any, Callable, Optional, TypeVar, Union

T = TypeVar('T')

@contextmanager
def isolate_imports(package_path: str):
    """
    Context manager that isolates imports for the given package path.
    
    Args:
        package_path: Path to the package to isolate imports for
        
    Yields:
        An ImportRunner object that can be used to execute code in the isolated environment
    """
    runner_id = None
    try:
        runner_id = start_import_runner_rs(package_path)
        yield ImportRunner(runner_id)
    finally:
        if runner_id:
            stop_import_runner_rs(runner_id)

class ImportRunner:
    """
    A class that represents an isolated Python environment for executing code.
    """
    def __init__(self, runner_id: str):
        """
        Initialize the ImportRunner with a runner ID.
        
        Args:
            runner_id: The unique identifier for this runner
        """
        self.runner_id = runner_id
        
    def exec(self, func: Callable[..., T], *args: Any) -> T:
        """
        Execute a function in the isolated environment.
        
        Args:
            func: The function to execute. A function should fully contain its content, including imports.
            *args: Arguments to pass to the function
            
        Returns:
            The result of the function execution
        """
        return exec_isolated_rs(self.runner_id, func, args if args else None)

    def communicate_isolated(self, process_uuid: str) -> str:
        """
        Communicate with an isolated process to get its output
        """
        return communicate_isolated_rs(self.runner_id, process_uuid)
