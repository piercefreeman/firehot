import importlib
from pathlib import Path

from firehot.context import resolve_package_metadata


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
