"""
Call serializer for hotreload.

Intended for embeddable usage in Rust, can only import stdlib modules. This logic is also injected into
the running process with pyo3, with an empty locals/global dict, so we should do all logic in global scope
without sub-functions.

"""

import base64
import inspect
import pickle
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from firehot.embedded.types import SerializedCall

    def func(val: int):
        pass

    args = (0,)

func_module_path_raw = None
func_file_path = "null"

if hasattr(func, "__module__"):
    module_name = func.__module__
    if module_name != "__main__":
        func_module_path_raw = module_name
    else:
        # Handle functions from directly executed scripts
        try:
            # Get the file where the function is defined
            file_path = inspect.getfile(func)
            raise RuntimeError(
                f"Function belongs to script, currently only modules are supported: {file_path}"
            )
        except (TypeError, ValueError):
            pass

# Slightly more manual approach to have full control over module loading when we run the
# function in our isolated environment.
payload: "SerializedCall" = {
    "func_module_path": func_module_path_raw,
    "func_name": func.__name__,
    "func_qualname": func.__qualname__,
    "args": args,
}

#
# Exports
# These variables are outputted into the local scope and read by Rust
#

pickled_data = base64.b64encode(pickle.dumps(payload)).decode("utf-8")
