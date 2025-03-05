"""
Call serializer for hotreload.

Intended for embeddable usage in Rust, can only import stdlib modules.

"""
import inspect, sys, os.path
from typing import Callable

# This will be passed in from rust
func: Callable

def get_func_module_path(func):
    if hasattr(func, '__module__'):
        module_name = func.__module__
        if module_name != '__main__':
            return module_name, None
        else:
            # Handle functions from directly executed scripts
            try:
                # Get the file where the function is defined
                file_path = inspect.getfile(func)
                if file_path and os.path.exists(file_path):
                    # Store the file path in a separate variable
                    return None, file_path
            except (TypeError, ValueError):
                pass
    return None, None

# Final string conversions, expected output values
func_module_path_raw, func_file_path_raw = get_func_module_path(func)
func_module_path = func_module_path_raw if func_module_path_raw is not None else "null"
func_file_path = func_file_path_raw if func_file_path_raw is not None else "null"
