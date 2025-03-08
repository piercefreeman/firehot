import sys

import pytest


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
