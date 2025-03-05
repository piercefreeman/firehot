"""
Call serializer for hotreload.

Intended for embeddable usage in Rust, can only import stdlib modules. This logic is also injected into
the running process with pyo3, with an empty locals/global dict, so we should do all logic in global scope
without sub-functions.

"""
from typing import Callable
import inspect
import os.path

# This will be passed in from rust
func: Callable

func_module_path_raw = None
func_file_path_raw = None

if hasattr(func, '__module__'):
    module_name = func.__module__
    if module_name != '__main__':
        func_module_path_raw = module_name
    else:
        # Handle functions from directly executed scripts
        try:
            # Get the file where the function is defined
            file_path = inspect.getfile(func)
            if file_path and os.path.exists(file_path):
                # Store the file path in a separate variable
                func_file_path_raw = file_path
        except (TypeError, ValueError):
            pass

# Final string conversions, expected output values
func_module_path = func_module_path_raw if func_module_path_raw is not None else "null"
func_file_path = func_file_path_raw if func_file_path_raw is not None else "null"
