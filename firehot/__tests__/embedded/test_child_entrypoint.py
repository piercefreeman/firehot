import base64
import importlib.util
import os
import pickle
import runpy
import sys

import pytest

from firehot.embedded.types import SerializedCall


@pytest.fixture
def child_entrypoint_file(tmp_path):
    spec = importlib.util.find_spec("firehot.embedded.child_entrypoint")
    if spec is not None and spec.origin and os.path.exists(spec.origin):
        return spec.origin
    raise Exception("Child entrypoint not found")


def test_module_usage_child_entrypoint(tmp_path, monkeypatch, child_entrypoint_file):
    """
    Test the branch where module_path is provided.

    A temporary module (dummy_module.py) is created with a simple function.
    We then import it normally so that its function is picklable.
    The pickled tuple contains (dummy_func, None) so that dummy_func() is called.
    """
    # Create a temporary module file (dummy_module.py)
    module_code = """
def dummy_func():
    return "module_result"
"""
    module_file = tmp_path / "dummy_module.py"
    module_file.write_text(module_code)

    # Prepend tmp_path to sys.path so that dummy_module can be imported
    monkeypatch.syspath_prepend(str(tmp_path))

    # Make sure we actually have to load the module from scratch
    if "dummy_module" in sys.modules:
        del sys.modules["dummy_module"]

    # Prepare pickled string: pack (dummy_func, None)
    payload: SerializedCall = {
        "func_module_path": "dummy_module",
        "func_name": "dummy_func",
        "args": None,
    }
    pickled_bytes = pickle.dumps(payload)
    pickled_str = base64.b64encode(pickled_bytes).decode("utf-8")

    # Prepare globals for the child entrypoint.
    entry_globals = {
        "pickled_str": pickled_str,
    }

    # Run the child entrypoint file.
    result_globals = runpy.run_path(child_entrypoint_file, init_globals=entry_globals)

    # The dummy_func returns "module_result"
    assert result_globals.get("result") == "module_result"
