import importlib
from pathlib import Path

from firehot.context import isolate_imports, resolve_package_metadata


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


def test_isolate_imports(sample_package):
    """
    Test that isolate_imports correctly creates an isolated environment and accepts ignored_modules.
    """
    # Test basic functionality without ignored modules
    with isolate_imports(sample_package) as env:
        assert env.runner_id is not None, "Expected runner_id to be set"

    # Test with ignored modules
    ignored = ["numpy", "pandas"]
    with isolate_imports(sample_package, ignored_modules=ignored) as env:
        assert env.runner_id is not None, "Expected runner_id to be set"
        # The runner_id should be different from the previous one
        assert isinstance(env.runner_id, str), "Expected runner_id to be a string"
