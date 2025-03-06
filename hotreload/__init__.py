"""
Hot Reload - A Python package with Rust extensions.

This package provides tools for isolating imports and executing code in forked processes
to avoid reloading the entire application during development.
"""

import importlib.util
from contextlib import contextmanager
from typing import Any, Callable, TypeVar
from uuid import UUID

from hotreload.hotreload import (
    communicate_isolated as communicate_isolated_rs,
)
from hotreload.hotreload import (
    exec_isolated as exec_isolated_rs,
)
from hotreload.hotreload import (
    start_import_runner as start_import_runner_rs,
)
from hotreload.hotreload import (
    stop_import_runner as stop_import_runner_rs,
)
from hotreload.hotreload import (
    update_environment as update_environment_rs,
)

T = TypeVar("T")


@contextmanager
def isolate_imports(package: str):
    """
    Context manager that isolates imports for the given package path.

    :param package: Package to isolate imports. This must be importable from the current
      virtual environment.

    Yields:
        An ImportRunner object that can be used to execute code in the isolated environment
    """
    # We need to resolve the package to a path
    spec = importlib.util.find_spec(package)
    if spec is None:
        raise ImportError(f"Could not find the package '{package}'")

    package_path = spec.origin
    package_name = spec.name
    if package_path is None:
        # For namespace packages
        if spec.submodule_search_locations:
            package_path = spec.submodule_search_locations[0]
        else:
            raise ImportError(f"Could not determine the path for package '{package}'")

    runner_id: str | None = None
    try:
        runner_id = start_import_runner_rs(package_name, package_path)
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

    def exec(self, func: Callable, *args: Any) -> UUID:
        """
        Execute a function in the isolated environment.

        Args:
            func: The function to execute. A function should fully contain its content, including imports.
            *args: Arguments to pass to the function

        Returns:
            The result of the function execution
        """
        return UUID(exec_isolated_rs(self.runner_id, func, args if args else None))

    def communicate_isolated(self, process_uuid: UUID) -> str:
        """
        Communicate with an isolated process to get its output
        """
        return communicate_isolated_rs(self.runner_id, str(process_uuid))

    def update_environment(self):
        """
        Update the environment by checking for import changes and restarting if necessary
        """
        return update_environment_rs(self.runner_id)
