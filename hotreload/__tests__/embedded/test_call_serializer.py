import importlib.util
import os
import runpy

import pytest


@pytest.fixture
def call_serializer_file(tmp_path):
    spec = importlib.util.find_spec("hotreload.embedded.call_serializer")
    if spec is not None and spec.origin and os.path.exists(spec.origin):
        return spec.origin
    raise Exception("Child entrypoint not found")


@pytest.fixture
def dummy_module(tmp_path, monkeypatch):
    """
    Create a temporary module file named dummy_module.py with a simple function.
    Add the temporary directory to sys.path so the module can be imported.
    """
    module_code = """
def dummy_func():
    return "dummy"
"""
    module_file = tmp_path / "dummy_module.py"
    module_file.write_text(module_code)

    # Prepend tmp_path to sys.path so that our temporary module is found.
    monkeypatch.syspath_prepend(str(tmp_path))

    # Dynamically import the temporary module.
    spec = importlib.util.spec_from_file_location("dummy_module", str(module_file))
    assert spec is not None

    dummy_module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None

    spec.loader.exec_module(dummy_module)
    return dummy_module


def test_module_usage(dummy_module, call_serializer_file):
    """
    When a function comes from a module (i.e. __module__ != '__main__'),
    get_func_module_path should return the module name and None for the file path.

    This test runs the call serializer file in a separate context, injecting
    dummy_module.dummy_func as the "func" global.
    """
    dummy_func = dummy_module.dummy_func
    result = runpy.run_path(call_serializer_file, init_globals={"func": dummy_func, "args": None})

    # In this case, dummy_func is defined in a proper module ("dummy_module"),
    # so get_func_module_path should return ("dummy_module", None),
    # and the call serializer file converts None into "null".
    assert result["func_module_path"] == "dummy_module"
    assert result["func_file_path"] == "null"


def test_independent_script_usage(tmp_path, call_serializer_file):
    """
    For an independently executed script (i.e. run as __main__),
    get_func_module_path should return (None, file_path) where file_path is the script's path.

    This test creates a temporary script that defines a dummy function, executes it as __main__,
    and then runs the call serializer file (in a separate context) with that function.
    """
    script_code = """
def dummy_func():
    return "dummy"
"""
    script_file = tmp_path / "temp_script.py"
    script_file.write_text(script_code)

    # Run the temporary script as __main__ to get the function.
    script_globals = runpy.run_path(str(script_file), run_name="__main__")
    dummy_func = script_globals["dummy_func"]

    with pytest.raises(RuntimeError):
        runpy.run_path(call_serializer_file, init_globals={"func": dummy_func})
