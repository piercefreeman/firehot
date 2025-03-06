import importlib
import sys
from pathlib import Path

import pytest

from firehot.environment import resolve_package_metadata


@pytest.fixture
def sample_package(tmp_path):
    """
    Create a sample package structure in a temporary directory
    and add it to sys.path for importing.

    Returns the path to the package.
    """
    # Create package structure
    package_name = "sample_package"
    package_dir = tmp_path / package_name
    package_dir.mkdir()

    # Create __init__.py file
    init_file = package_dir / "__init__.py"
    init_file.write_text("# Sample package __init__ file")

    # Create a module inside the package
    module_file = package_dir / "module.py"
    module_file.write_text("def sample_function():\n    return 'Hello, world!'")

    # Add tmp_path to sys.path so the package can be imported
    sys.path.insert(0, str(tmp_path))

    yield package_name

    # Clean up - remove from sys.path
    if str(tmp_path) in sys.path:
        sys.path.remove(str(tmp_path))


def test_resolve_package_metadata(sample_package):
    """
    Test that resolve_package_metadata correctly resolves the package root path
    and not a file like __init__.py
    """
    # Import the package to ensure it's in sys.modules
    importlib.import_module(sample_package)

    # Resolve the package metadata
    package_path, package_name = resolve_package_metadata(sample_package)

    # Assertions
    assert package_name == sample_package

    # Check that package_path is a directory (not a file like __init__.py)
    resolved_path = Path(package_path)
    assert resolved_path.is_dir(), f"Expected directory path, got: {package_path}"

    # Check that the directory contains the expected files
    assert (resolved_path / "__init__.py").exists()
    assert (resolved_path / "module.py").exists()

    # Check that the directory name matches the package name
    assert resolved_path.name == sample_package
