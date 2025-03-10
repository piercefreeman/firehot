import importlib.util
from contextlib import contextmanager
from pathlib import Path

from firehot.environment import Environment
from firehot.firehot import (
    start_import_runner as start_import_runner_rs,
)
from firehot.firehot import (
    stop_import_runner as stop_import_runner_rs,
)


def resolve_package_metadata(package: str) -> tuple[str, str]:
    """
    Resolve the package path and name.

    :param package: Package to resolve
    :returns: A tuple containing package path and package name
    :raises ImportError: If the package cannot be found or its path cannot be determined
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

    # We care about the root path, not a file. If we were returned __init__.py, we should
    # use the directory instead.
    if Path(package_path).is_file():
        package_path = str(Path(package_path).parent)

    return package_path, package_name


@contextmanager
def isolate_imports(package: str):
    """
    Context manager that isolates imports for the given package path.

    :param package: Package to isolate imports. This must be importable from the current
                    virtual environment
    :yields: An Environment object that can be used to execute code in the isolated environment
    """
    package_path, package_name = resolve_package_metadata(package)
    runner_id: str | None = None
    try:
        runner_id = start_import_runner_rs(package_name, package_path)
        yield Environment(runner_id)
    finally:
        if runner_id:
            stop_import_runner_rs(runner_id)
